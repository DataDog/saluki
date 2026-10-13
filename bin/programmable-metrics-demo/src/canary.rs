//! The canary conservation check: whether the primary endpoint received every canary the traffic generator sent.
//!
//! Every datagram carries one `demo.canary:1|c`, of one of the canary's contexts (its shards, which spread it over
//! aggregation's state lanes: see [`traffic`](crate::traffic)), and the generator counts the canaries it sends in each
//! second. ADP's aggregation assigns each canary to the window it arrived in and sends the window's count with the
//! window's start as its timestamp, so the canaries an intake received with timestamps in a range of seconds, of every
//! shard, are the ones sent in those seconds. The check compares the two over a recent range ([`window`]) that every
//! flush has had time to deliver, so it follows the present: a loss shows while its seconds are in the range and leaves
//! with them, and canaries sent before ADP was listening never count against it.
//!
//! The range is aligned to the canary's buckets: to the configured aggregation window, and, while an artifact of the
//! pre-aggregation scope sets another window for the aggregation (a width change) and for the five minutes of history
//! after it stops being served, to the least common multiple of the widths in use ([`artifact_widths`],
//! [`common_width`]). A range aligned to it holds whole buckets of every width, before a width change's cut, after it,
//! and after the artifact is revoked again, since a cut takes effect at a multiple of the least common multiple of the
//! two widths.
//!
//! Canaries the primary endpoint did not receive are lost in ADP only if ADP received them. The host can drop
//! datagrams before ADP's DogStatsD source reads them, as macOS drops a share of every loopback socket's datagrams
//! while another process on the host floods UDP, and no counter of ADP's sees those. So a range short of canaries is
//! checked against how its datagrams reached ADP ([`Delivery`]): if the host dropped datagrams in those seconds, as the
//! demo's own control datagrams show ([`loopback`](crate::loopback)), and ADP's DogStatsD source received fewer
//! datagrams than were sent by as many as the canaries missing (to within the allowance, and a second's canaries that
//! the overloaded host lets ADP admit late: [`HOST_DROP_LAG_SECS`]), the host dropped them
//! ([`CanaryState::HostDropped`]), which is no loss of ADP's. Otherwise the canaries are missing, and the check says
//! whether ADP's source received their datagrams.

use saluki_metric_plan::ir::parse_duration;
use serde::Serialize;
use serde_json::Value;

use crate::{loopback::ControlTally, traffic::CANARY_HISTORY_SECS};

/// The operators whose `window` parameter sets the width of the buckets a canary is counted in.
const WINDOWED_OPERATORS: [&str; 2] = ["aggregate", "timestamp_split"];

/// The scope whose artifacts can set the width of the canary's buckets: the pre-aggregation scope, which holds
/// DogStatsD aggregation. ADP refuses an aggregation in any other scope, so another scope's windows never take effect.
pub const WIDTH_SCOPE: &str = "dogstatsd_pre_aggregation";

/// Returns the widths, in whole seconds, that an artifact's aggregations (and the timestamp splits ahead of them) set
/// with a literal window: the bucket widths the artifact gives the canary, if it is of [`WIDTH_SCOPE`]. A window that
/// refers to the configuration, which is the configured window, and one that is not a whole number of seconds are left
/// out.
pub fn artifact_widths(artifact: &Value) -> Vec<u64> {
    artifact["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|node| node["op"].as_str().is_some_and(|op| WINDOWED_OPERATORS.contains(&op)))
        .filter_map(|node| parse_duration(node["params"]["window"].as_str()?))
        .filter(|window| window.subsec_nanos() == 0 && window.as_secs() > 0)
        .map(|window| window.as_secs())
        .collect()
}

/// Returns the least common multiple of two widths in seconds, saturating at `u64::MAX`.
pub fn lcm(a: u64, b: u64) -> u64 {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    match (a, b) {
        (0, other) | (other, 0) => other,
        (a, b) => (a / gcd(a, b)).saturating_mul(b),
    }
}

/// The widest width the check aligns its ranges to for the widths artifacts set: the range of a wider one would start
/// so long ago that its seconds leave the history the demo keeps ([`CANARY_HISTORY_SECS`]) before they are checked. A
/// configured window wider than this is still aligned to.
pub const MAX_COMMON_WIDTH_SECS: u64 = 60;

/// Returns the width the check's ranges are aligned to, for buckets of the configured window and of every width in
/// `widths` that artifacts set: their least common multiple. Returns `None` if that is wider than both
/// [`MAX_COMMON_WIDTH_SECS`] and the configured window, so that no range the check can read holds whole buckets of
/// every width (windows of 7 and 60 seconds, say, which only every 420 seconds share a bucket boundary).
pub fn common_width(configured_secs: u64, widths: impl IntoIterator<Item = u64>) -> Option<u64> {
    let configured = configured_secs.max(1);
    let width = widths.into_iter().fold(configured, lcm);
    (width <= configured.max(MAX_COMMON_WIDTH_SECS)).then_some(width)
}

/// How far behind the present the range ends, beyond one aggregation window, in seconds: the time a window's flush
/// takes to reach the intakes (the aggregation flush, the encoder's batching, and the forwarder).
pub const LAG_SECS: u64 = 6;

/// How long the range is, in seconds (rounded up to whole aggregation windows).
pub const SPAN_SECS: u64 = 10;

/// How many canaries may be counted in a second next to the one they were sent in, per datagram per second sent.
///
/// ADP stamps a canary when it aggregates it, a moment after the generator counted it, so a canary sent at the very
/// end of a second can be counted in the next one. That moves canaries across the ends of the range, which this allows
/// for; a payload lost in transit loses a whole second of canaries.
const EDGE_ALLOWANCE: f64 = 0.1;

/// How many seconds late ADP may admit the canaries of datagrams it received while the host was dropping datagrams.
///
/// A host that drops loopback datagrams is overloaded, and ADP then reads the datagrams it does receive late, so up to
/// that long's canaries of a range are counted in the next one: the shortfall of a range in which the host dropped
/// datagrams is the host's if the datagrams ADP's source did not receive account for it to within the allowance and
/// that many seconds' canaries.
const HOST_DROP_LAG_SECS: f64 = 1.0;

/// What the check found.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryState {
    /// The check cannot say yet: ADP has not been up for the whole range, or canaries are missing while the host was
    /// dropping datagrams, and no scrape of ADP's telemetry since the range ended says yet whether ADP received them.
    Waiting,

    /// Nothing was sent in the range, and nothing received.
    Idle,

    /// The primary endpoint received what was sent, within the allowance.
    Balanced,

    /// The primary endpoint received fewer canaries than were sent: some were lost.
    Missing,

    /// The primary endpoint received fewer canaries than were sent, and the host dropped them before ADP read them:
    /// ADP's DogStatsD source received as many fewer datagrams than were sent, to within the allowance and a second's
    /// canaries admitted late ([`HOST_DROP_LAG_SECS`]), and the host dropped some of the demo's control datagrams of
    /// those seconds.
    HostDropped,

    /// The primary endpoint received more canaries than were sent: some were counted twice.
    Excess,
}

impl CanaryState {
    /// Returns the state's name, as the demo's API writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Balanced => "balanced",
            Self::Missing => "missing",
            Self::HostDropped => "host_dropped",
            Self::Excess => "excess",
        }
    }
}

/// How the datagrams of a range reached ADP: what ADP's DogStatsD source received of them, and what the host dropped of
/// the demo's control datagrams in those seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Delivery {
    /// The datagrams sent between the scrapes of ADP's telemetry around the range that ADP's DogStatsD source did not
    /// receive, or `None` if no scrapes bracket the range yet. The scrapes bracket up to a scrape interval more than
    /// the range on each side, more if a scrape failed, and sends are counted per second, so this is off by a datagram
    /// or two at each end: it bounds what the host dropped in the range, generously, rather than counting it.
    pub not_received: Option<f64>,

    /// The control datagrams sent in the range and in the second on each side of it, and how many of them arrived, if
    /// the traffic generator sends any.
    pub control: Option<ControlTally>,
}

impl Delivery {
    /// Returns `true` if the host dropped any of the demo's control datagrams.
    fn host_dropped(&self) -> bool {
        self.control.is_some_and(|control| control.lost() > 0)
    }
}

/// The result of the check.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanaryCheck {
    /// What the check found.
    pub state: CanaryState,

    /// Why the check cannot say yet, when it is waiting; and where the canaries went, when some are missing or the host
    /// dropped them.
    pub reason: Option<String>,

    /// The first second of the range (Unix seconds).
    pub from_s: u64,

    /// The second just past the range.
    pub to_s: u64,

    /// Canaries sent in the range.
    pub sent: u64,

    /// Canaries the primary endpoint received with timestamps in the range.
    pub received: f64,

    /// How far `received` may be from `sent` and still balance.
    pub allowance: f64,

    /// Canaries other endpoints received with timestamps in the range, by intake name.
    pub elsewhere: Vec<CanaryElsewhere>,

    /// How the range's datagrams reached ADP.
    pub delivery: Delivery,

    /// Whether the check waits for a scrape of ADP's telemetry, so that the same range read again later can say more.
    pub pending_telemetry: bool,
}

/// Canaries an endpoint other than the primary one received.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanaryElsewhere {
    /// The intake's name.
    pub intake: &'static str,

    /// The canaries it received with timestamps in the range.
    pub received: f64,
}

/// Returns the latest second a range of seconds may end at, at `now_ms`, for its last bucket to have reached the
/// intakes, if the range ends at a bucket boundary: a range must end at least [`LAG_SECS`] seconds and one second ago.
pub fn latest_delivered(now_ms: u64) -> u64 {
    (now_ms / 1000).saturating_sub(1 + LAG_SECS)
}

/// Checks that the check can say what a range of seconds, `(from_s, to_s)`, holds at `now_ms`: that it is a nonempty
/// range within the last [`CANARY_HISTORY_SECS`] seconds, the counts the demo keeps, ending by [`latest_delivered`].
///
/// # Errors
///
/// Returns the earliest second a range may start at and the latest it may end at, if the range is not one.
pub fn readable((from_s, to_s): (u64, u64), now_ms: u64) -> Result<(), (u64, u64)> {
    let oldest = (now_ms / 1000).saturating_sub(CANARY_HISTORY_SECS);
    let latest = latest_delivered(now_ms);
    if from_s < to_s && from_s >= oldest && to_s <= latest {
        Ok(())
    } else {
        Err((oldest, latest))
    }
}

/// Returns the range of seconds the check covers at `now_ms`, as `(from_s, to_s)`, aligned to aggregation windows of
/// `window_secs` seconds so that each window is in it whole or not at all.
pub fn window(now_ms: u64, window_secs: u64) -> (u64, u64) {
    let width = window_secs.max(1);
    let to_s = ((now_ms / 1000).saturating_sub(width + LAG_SECS) / width) * width;
    let span = SPAN_SECS.div_ceil(width) * width;
    (to_s.saturating_sub(span), to_s)
}

/// Returns what the primary endpoint receiving `received` of `sent` canaries says: idle if nothing was sent or
/// received, balanced within `allowance`, and otherwise missing or in excess.
pub fn classify(sent: u64, received: f64, allowance: f64) -> CanaryState {
    let difference = received - sent as f64;
    if sent == 0 && received == 0.0 {
        CanaryState::Idle
    } else if difference.abs() <= allowance {
        CanaryState::Balanced
    } else if difference < 0.0 {
        CanaryState::Missing
    } else {
        CanaryState::Excess
    }
}

/// Returns what the sums of a whole run's contiguous ranges say, of whose canaries the host dropped `host_dropped` (the
/// shortfall of its ranges found [`HostDropped`](CanaryState::HostDropped)): what [`classify`] says, except that a
/// shortfall that the host's drops account for, within `allowance`, is [`HostDropped`](CanaryState::HostDropped).
pub fn classify_run(sent: u64, received: f64, host_dropped: f64, allowance: f64) -> CanaryState {
    match classify(sent, received, allowance) {
        CanaryState::Missing if host_dropped > 0.0 && sent as f64 - received - host_dropped <= allowance => {
            CanaryState::HostDropped
        }
        state => state,
    }
}

/// Returns what a shortfall of `shortfall` canaries beyond the allowance is, as `delivery` says: dropped by the host,
/// missing, or, while the host was dropping datagrams and the source's count does not cover the range yet, not known
/// yet; and where the canaries went, if the source's count says. `rate` is the generator's datagram rate, which sets
/// how many canaries ADP may admit late while the host drops datagrams ([`HOST_DROP_LAG_SECS`]).
fn attribute(shortfall: f64, allowance: f64, rate: f64, delivery: &Delivery) -> (CanaryState, Option<String>) {
    let Some(not_received) = delivery.not_received else {
        return match delivery.control {
            Some(control) if delivery.host_dropped() => (
                CanaryState::Waiting,
                Some(format!(
                    "The host dropped {} of the demo's {} control datagrams of these seconds, and no scrape of ADP's \
                     telemetry since the range ended says yet whether ADP's DogStatsD source received the datagrams of \
                     the {:.0} canaries missing.",
                    control.lost(),
                    control.sent,
                    shortfall
                )),
            ),
            _ => (CanaryState::Missing, None),
        };
    };
    let not_received = not_received.max(0.0);
    let late = if delivery.host_dropped() {
        rate * HOST_DROP_LAG_SECS
    } else {
        0.0
    };
    if not_received + allowance + late < shortfall {
        return (
            CanaryState::Missing,
            Some(format!(
                "ADP's DogStatsD source received all but about {:.0} of the datagrams sent: the other {:.0} canaries \
                 were lost in ADP.",
                not_received,
                shortfall - not_received
            )),
        );
    }
    match delivery.control {
        Some(control) if delivery.host_dropped() => (
            CanaryState::HostDropped,
            Some(format!(
                "The host dropped the datagrams of the {:.0} canaries missing before ADP read them: ADP's DogStatsD \
                 source received about {:.0} fewer datagrams than were sent, and the host dropped {} of the demo's {} \
                 control datagrams of those seconds. Another process on this host may be flooding UDP. ADP lost none \
                 of the canaries it received.",
                shortfall,
                not_received,
                control.lost(),
                control.sent
            )),
        ),
        Some(control) => (
            CanaryState::Missing,
            Some(format!(
                "ADP's DogStatsD source received about {:.0} fewer datagrams than were sent, while the host \
                 delivered all {} of the demo's control datagrams of those seconds: ADP did not read its socket in \
                 time.",
                not_received, control.sent
            )),
        ),
        None => (
            CanaryState::Missing,
            Some(format!(
                "ADP's DogStatsD source received about {:.0} fewer datagrams than were sent.",
                not_received
            )),
        ),
    }
}

/// Compares what was sent with what the primary endpoint received over a range.
///
/// `rate` is the generator's datagram rate, `up_since_ms` when ADP began answering without interruption (`None` if it
/// does not answer now), and `delivery` how the range's datagrams reached ADP, which decides whether canaries missing
/// beyond the allowance were dropped by the host ([`attribute`]).
pub fn check(
    (from_s, to_s): (u64, u64), sent: u64, received: f64, elsewhere: Vec<CanaryElsewhere>, rate: f64,
    up_since_ms: Option<u64>, delivery: Delivery,
) -> CanaryCheck {
    let allowance = 2.0 + 2.0 * EDGE_ALLOWANCE * rate;
    let waiting = match up_since_ms {
        None => Some("ADP is not answering.".to_string()),
        Some(since) if since > from_s * 1000 => {
            Some("ADP has not been up for the whole range of seconds the check covers yet.".to_string())
        }
        Some(_) => None,
    };
    let (state, reason, pending_telemetry) = match waiting {
        Some(waiting) => (CanaryState::Waiting, Some(waiting), false),
        None => match classify(sent, received, allowance) {
            CanaryState::Missing => {
                let (state, reason) = attribute(sent as f64 - received, allowance, rate, &delivery);
                (state, reason, state == CanaryState::Waiting)
            }
            state => (state, None, false),
        },
    };
    CanaryCheck {
        state,
        reason,
        from_s,
        to_s,
        sent,
        received,
        allowance,
        elsewhere,
        delivery,
        pending_telemetry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_ends_behind_the_present_and_holds_whole_windows() {
        // At 1000.5 s with 1-second windows: ends at 1000 - 1 - 6 = 993, ten seconds long.
        assert_eq!(window(1_000_500, 1), (983, 993));
        // With 10-second windows: 1000 - 10 - 6 = 984, down to a window boundary, 980; one window long.
        assert_eq!(window(1_000_500, 10), (970, 980));
        // With 3-second windows: 1000 - 3 - 6 = 991, down to 990; ten seconds round up to four windows.
        assert_eq!(window(1_000_000, 3), (978, 990));
        assert_eq!(window(0, 1), (0, 0));
        // With 30-second buckets, the least common multiple of 10 and 30 seconds after a width change: 1000 - 30 - 6
        // = 964, down to 960, one window long.
        assert_eq!(window(1_000_500, 30), (930, 960));
        // A range ending at a bucket boundary is delivered seven seconds later: what one-second windows always allowed.
        assert_eq!(latest_delivered(1_000_500), 993);
        assert_eq!(latest_delivered(1_000_500), window(1_000_500, 1).1);
        assert_eq!(latest_delivered(3_000), 0);
    }

    #[test]
    fn an_artifacts_literal_windows_are_the_widths_it_gives_the_canary() {
        let artifact = serde_json::json!({
            "nodes": [
                {"id": "dsd_no_agg_split", "op": "timestamp_split", "params": {"window": "30s"}},
                {"id": "dsd_agg", "op": "aggregate", "params": {"window": "1m", "flush_interval": "1s"}},
                {"id": "configured", "op": "aggregate",
                 "params": {"window": {"config": "dogstatsd.aggregation.window_duration_seconds"}}},
                {"id": "fraction", "op": "aggregate", "params": {"window": "1500ms"}},
                {"id": "other", "op": "tag_projection", "params": {"window": "7s"}}
            ]
        });
        assert_eq!(artifact_widths(&artifact), [30, 60]);
        assert!(artifact_widths(&serde_json::json!({"nodes": "none"})).is_empty());

        assert_eq!(lcm(10, 30), 30);
        assert_eq!(lcm(4, 6), 12);
        assert_eq!(lcm(1, 10), 10);
        assert_eq!(lcm(0, 10), 10);
        assert_eq!(lcm(u64::MAX, u64::MAX - 1), u64::MAX);
    }

    #[test]
    fn the_check_balances_within_the_allowance_and_names_losses_and_doubles() {
        let range = (983, 993);
        let up = Some(900_000);
        // At 20 datagrams per second, the allowance is 2 + 2 × 0.1 × 20 = 6.
        let cases = [
            (200, 200.0, CanaryState::Balanced),
            (200, 195.0, CanaryState::Balanced),
            (200, 206.0, CanaryState::Balanced),
            (200, 180.0, CanaryState::Missing),
            (200, 220.0, CanaryState::Excess),
            (0, 0.0, CanaryState::Idle),
            (0, 20.0, CanaryState::Excess),
        ];
        for (sent, received, expected) in cases {
            let result = check(range, sent, received, Vec::new(), 20.0, up, Delivery::default());
            assert_eq!(result.state, expected, "sent {} received {}", sent, received);
            assert_eq!(result.allowance, 6.0);
            assert_eq!(result.reason, None);
        }

        // ADP down, or up only since within the range: the check waits rather than call canaries missing.
        let down = check(range, 200, 0.0, Vec::new(), 20.0, None, Delivery::default());
        assert_eq!(down.state, CanaryState::Waiting);
        assert!(down.reason.unwrap().contains("not answering"));
        let late = check(range, 200, 40.0, Vec::new(), 20.0, Some(985_000), Delivery::default());
        assert_eq!(late.state, CanaryState::Waiting);
        assert_eq!(
            check(range, 200, 200.0, Vec::new(), 20.0, Some(983_000), Delivery::default()).state,
            CanaryState::Balanced
        );
        // Each state's name is the one the API writes.
        for state in [
            CanaryState::Waiting,
            CanaryState::Idle,
            CanaryState::Balanced,
            CanaryState::Missing,
            CanaryState::HostDropped,
            CanaryState::Excess,
        ] {
            assert_eq!(serde_json::json!(state), state.name());
        }
    }

    #[test]
    fn canaries_the_host_dropped_before_adp_read_them_are_not_lost_in_adp() {
        // At 20 datagrams per second the allowance is 6.
        let range = (983, 993);
        let up = Some(900_000);
        let delivery = |not_received: Option<f64>, control: Option<(u64, u64)>| Delivery {
            not_received,
            control: control.map(|(sent, received)| ControlTally { sent, received }),
        };
        let state_of = |received: f64, delivery: Delivery| {
            let check = check(range, 200, received, Vec::new(), 20.0, up, delivery);
            (check.state, check.reason.unwrap_or_default())
        };

        // ADP's source never received 31 datagrams (one more than the canaries missing, which the scrapes around the
        // range allow for), and the host dropped five of the demo's 240 control datagrams: the host dropped them.
        let (state, reason) = state_of(170.0, delivery(Some(31.0), Some((240, 235))));
        assert_eq!(state, CanaryState::HostDropped);
        assert!(reason.contains("The host dropped"), "{}", reason);
        assert!(reason.contains("5 of the demo's 240 control datagrams"), "{}", reason);
        // The source's count, a datagram or two off at each end, may fall short of the canaries missing by up to the
        // allowance, and, while the host drops datagrams, by a second's canaries more, which ADP admits late.
        assert_eq!(
            state_of(170.0, delivery(Some(24.0), Some((240, 239)))).0,
            CanaryState::HostDropped
        );
        assert_eq!(
            state_of(170.0, delivery(Some(4.0), Some((240, 239)))).0,
            CanaryState::HostDropped
        );
        assert_eq!(
            state_of(170.0, delivery(Some(3.0), Some((240, 239)))).0,
            CanaryState::Missing
        );

        // The source received all but two of 50 missing: the canaries were lost in ADP, whatever the host dropped.
        let (state, reason) = state_of(150.0, delivery(Some(2.0), Some((240, 230))));
        assert_eq!(state, CanaryState::Missing);
        assert!(reason.contains("the other 48 canaries were lost in ADP"), "{}", reason);
        // More missing than the source did not receive, beyond the allowance and a second's canaries: ADP lost them.
        assert_eq!(
            state_of(150.0, delivery(Some(20.0), Some((240, 220)))).0,
            CanaryState::Missing
        );
        // Without host drops, no canary counts as late: the source's count must account for the shortfall by itself.
        assert_eq!(
            state_of(170.0, delivery(Some(4.0), Some((240, 240)))).0,
            CanaryState::Missing
        );

        // The source did not receive them, but the host delivered every control datagram: ADP's socket overflowed.
        let (state, reason) = state_of(170.0, delivery(Some(30.0), Some((240, 240))));
        assert_eq!(state, CanaryState::Missing);
        assert!(reason.contains("ADP did not read its socket in time"), "{}", reason);
        // Without control datagrams, nothing says that the host dropped them.
        assert_eq!(state_of(170.0, delivery(Some(30.0), None)).0, CanaryState::Missing);
        assert_eq!(
            state_of(170.0, delivery(None, None)),
            (CanaryState::Missing, String::new())
        );
        // While the host was dropping, a range that no scrape since its end covers yet waits for one.
        let (state, reason) = state_of(170.0, delivery(None, Some((240, 230))));
        assert_eq!(state, CanaryState::Waiting);
        assert!(
            reason.contains("no scrape of ADP's telemetry since the range ended"),
            "{}",
            reason
        );
        assert!(
            check(
                range,
                200,
                170.0,
                Vec::new(),
                20.0,
                up,
                delivery(None, Some((240, 230)))
            )
            .pending_telemetry
        );
        // A range ADP was not up for is not one a later scrape can say more of.
        let late = check(
            range,
            200,
            170.0,
            Vec::new(),
            20.0,
            Some(990_000),
            delivery(None, Some((240, 230))),
        );
        assert!(!late.pending_telemetry);
        assert_eq!(
            state_of(170.0, delivery(None, Some((240, 240)))).0,
            CanaryState::Missing
        );

        // Within the allowance, or in excess, the delivery does not matter.
        assert_eq!(
            state_of(195.0, delivery(Some(5.0), Some((240, 235)))).0,
            CanaryState::Balanced
        );
        assert_eq!(
            state_of(220.0, delivery(Some(0.0), Some((240, 230)))).0,
            CanaryState::Excess
        );
        // A range ADP was not up for waits, as it did.
        let late = Some(990_000);
        let waiting = check(
            range,
            200,
            170.0,
            Vec::new(),
            20.0,
            late,
            delivery(Some(30.0), Some((240, 230))),
        );
        assert_eq!(waiting.state, CanaryState::Waiting);

        // A whole run: a shortfall that the host's drops account for, within the allowance, is the host's; one that
        // they do not account for is missing.
        assert_eq!(classify_run(3_600, 3_570.0, 30.0, 6.0), CanaryState::HostDropped);
        assert_eq!(classify_run(3_600, 3_570.0, 25.0, 6.0), CanaryState::HostDropped);
        assert_eq!(classify_run(3_600, 3_570.0, 20.0, 6.0), CanaryState::Missing);
        assert_eq!(classify_run(3_600, 3_570.0, 0.0, 6.0), CanaryState::Missing);
        assert_eq!(classify_run(3_600, 3_598.0, 0.0, 6.0), CanaryState::Balanced);
        assert_eq!(classify_run(3_600, 3_598.0, 30.0, 6.0), CanaryState::Balanced);
    }

    #[test]
    fn ranges_align_to_the_widths_in_use_while_their_common_boundaries_are_within_the_history() {
        // The configured window alone, and with the widths a width change sets.
        assert_eq!(common_width(1, []), Some(1));
        assert_eq!(common_width(0, []), Some(1));
        assert_eq!(common_width(10, [30]), Some(30));
        assert_eq!(common_width(10, [30, 10]), Some(30));
        assert_eq!(common_width(1, [60]), Some(60));
        // Windows of 7 and 60 seconds share a boundary every 420 seconds: no range the check can read holds whole
        // buckets of both.
        assert_eq!(common_width(1, [7, 60]), None);
        assert_eq!(common_width(10, [7]), None);
        // A configured window wider than that is aligned to, as it always was.
        assert_eq!(common_width(120, []), Some(120));
        assert_eq!(common_width(120, [30]), Some(120));
        // The range of the widest width aligned to for artifacts still starts within the history.
        let now_ms = 1_000_000_500;
        let (from_s, to_s) = window(now_ms, MAX_COMMON_WIDTH_SECS);
        assert!(readable((from_s, to_s), now_ms).is_ok());
    }

    #[test]
    fn a_range_is_readable_if_nonempty_within_the_history_and_delivered() {
        // At 1,000,500 s: the history starts at 1,000,200 and a range may end at 1,000,493.
        let now_ms = 1_000_500_700;
        assert_eq!(readable((1_000_200, 1_000_493), now_ms), Ok(()));
        assert_eq!(readable((1_000_480, 1_000_490), now_ms), Ok(()));
        for range in [
            (1_000_199, 1_000_400),
            (1_000_400, 1_000_494),
            (1_000_400, 1_000_400),
            (1_000_410, 1_000_400),
        ] {
            assert_eq!(readable(range, now_ms), Err((1_000_200, 1_000_493)), "{:?}", range);
        }
    }
}
