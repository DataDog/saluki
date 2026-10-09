//! The canary conservation check: whether the primary endpoint received every canary the traffic generator sent.
//!
//! Every datagram carries one `demo.canary:1|c`, and the generator counts the canaries it sends in each second.
//! ADP's aggregation assigns each canary to the window it arrived in and sends the window's count with the window's
//! start as its timestamp, so the canaries an intake received with timestamps in a range of seconds are the ones sent
//! in those seconds. The check compares the two over a recent range ([`window`]) that every flush has had time to
//! deliver, so it follows the present: a loss shows while its seconds are in the range and leaves with them, and
//! canaries sent before ADP was listening never count against it.

use serde::Serialize;

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

/// What the check found.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryState {
    /// The check cannot say yet: ADP has not been up for the whole range.
    Waiting,

    /// Nothing was sent in the range, and nothing received.
    Idle,

    /// The primary endpoint received what was sent, within the allowance.
    Balanced,

    /// The primary endpoint received fewer canaries than were sent: some were lost.
    Missing,

    /// The primary endpoint received more canaries than were sent: some were counted twice.
    Excess,
}

/// The result of the check.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanaryCheck {
    /// What the check found.
    pub state: CanaryState,

    /// Why the check cannot say yet, when it is waiting.
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
}

/// Canaries an endpoint other than the primary one received.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CanaryElsewhere {
    /// The intake's name.
    pub intake: &'static str,

    /// The canaries it received with timestamps in the range.
    pub received: f64,
}

/// Returns the range of seconds the check covers at `now_ms`, as `(from_s, to_s)`, aligned to aggregation windows of
/// `window_secs` seconds so that each window is in it whole or not at all.
pub fn window(now_ms: u64, window_secs: u64) -> (u64, u64) {
    let width = window_secs.max(1);
    let to_s = ((now_ms / 1000).saturating_sub(width + LAG_SECS) / width) * width;
    let span = SPAN_SECS.div_ceil(width) * width;
    (to_s.saturating_sub(span), to_s)
}

/// Compares what was sent with what the primary endpoint received over a range.
///
/// `rate` is the generator's datagram rate, and `up_since_ms` when ADP began answering without interruption (`None`
/// if it does not answer now).
pub fn check(
    (from_s, to_s): (u64, u64), sent: u64, received: f64, elsewhere: Vec<CanaryElsewhere>, rate: f64,
    up_since_ms: Option<u64>,
) -> CanaryCheck {
    let allowance = 2.0 + 2.0 * EDGE_ALLOWANCE * rate;
    let waiting = match up_since_ms {
        None => Some("ADP is not answering.".to_string()),
        Some(since) if since > from_s * 1000 => {
            Some("ADP has not been up for the whole range of seconds the check covers yet.".to_string())
        }
        Some(_) => None,
    };
    let difference = received - sent as f64;
    let state = match &waiting {
        Some(_) => CanaryState::Waiting,
        None if sent == 0 && received == 0.0 => CanaryState::Idle,
        None if difference.abs() <= allowance => CanaryState::Balanced,
        None if difference < 0.0 => CanaryState::Missing,
        None => CanaryState::Excess,
    };
    CanaryCheck {
        state,
        reason: waiting,
        from_s,
        to_s,
        sent,
        received,
        allowance,
        elsewhere,
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
            let result = check(range, sent, received, Vec::new(), 20.0, up);
            assert_eq!(result.state, expected, "sent {} received {}", sent, received);
            assert_eq!(result.allowance, 6.0);
            assert_eq!(result.reason, None);
        }

        // ADP down, or up only since within the range: the check waits rather than call canaries missing.
        let down = check(range, 200, 0.0, Vec::new(), 20.0, None);
        assert_eq!(down.state, CanaryState::Waiting);
        assert!(down.reason.unwrap().contains("not answering"));
        let late = check(range, 200, 40.0, Vec::new(), 20.0, Some(985_000));
        assert_eq!(late.state, CanaryState::Waiting);
        assert_eq!(
            check(range, 200, 200.0, Vec::new(), 20.0, Some(983_000)).state,
            CanaryState::Balanced
        );
    }
}
