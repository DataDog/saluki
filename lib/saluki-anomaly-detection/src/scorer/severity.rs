//! The severity-event contract: levels, transitions, filters, and listeners.
//!
//! This is a port of the Go `comp/anomalydetection/severityevents/def` package. The scorer derives a raw
//! severity level per second from its EWMA stream and pushes it through per-subscription dispatchers
//! ([`crate::scorer::Dispatcher`]); the types here are the vocabulary that crosses those boundaries.
//!
//! Unlike the Go package, the Rust port is single-threaded (the engine owns the scorer and drives it
//! synchronously), so the atomic pull-reader convenience from `severityevents/impl` is not needed.

use std::fmt::{self, Display, Formatter};

/// One of the three severity states: Low, Medium, or High.
///
/// The ordinals match the Go `severityevents.SeverityLevel` enum, and the ordering is meaningful:
/// escalation is a move to a higher ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SeverityLevel {
    /// The no-evidence baseline state.
    Low = 0,
    /// The EWMA is at or above the low threshold.
    Medium = 1,
    /// The EWMA is at or above the high threshold.
    High = 2,
}

impl SeverityLevel {
    /// Returns the canonical, low-cardinality name for the level (`low`, `medium`, `high`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl Display for SeverityLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The direction of a severity transition.
///
/// The ordinals match the Go `severityevents.SeverityEventDirection` enum; `Both` is the zero value that
/// matches either direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum SeverityEventDirection {
    /// Delivers transitions in either direction (the zero value).
    #[default]
    Both = 0,
    /// Delivers only transitions where the level increases.
    Escalation = 1,
    /// Delivers only transitions where the level decreases.
    Deescalation = 2,
}

/// Records a severity state-machine transition.
///
/// `direction` is [`SeverityEventDirection::Escalation`] when `to_level > from_level` and
/// [`SeverityEventDirection::Deescalation`] when `to_level < from_level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeverityEvent {
    /// The data time (Unix seconds) when the transition occurred.
    pub timestamp_sec: i64,
    /// The state before the transition.
    pub from_level: SeverityLevel,
    /// The state after the transition.
    pub to_level: SeverityLevel,
    /// Whether the transition escalated or de-escalated.
    pub direction: SeverityEventDirection,
}

/// Returns the direction of a transition from `from` to `to`, as the Go `eventDirection` helper does.
pub fn event_direction(from: SeverityLevel, to: SeverityLevel) -> SeverityEventDirection {
    if to > from {
        SeverityEventDirection::Escalation
    } else {
        SeverityEventDirection::Deescalation
    }
}

/// Selects which [`SeverityEvent`]s are delivered to a listener.
///
/// All conditions are ANDed; an empty level set means "any value". The default (zero-value) filter
/// matches every transition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeverityEventFilter {
    /// Restricts delivery to events whose from-level is in this set.
    pub from_levels: Vec<SeverityLevel>,
    /// Restricts delivery to events whose to-level is in this set.
    pub to_levels: Vec<SeverityLevel>,
    /// Restricts delivery by escalation or de-escalation.
    pub direction: SeverityEventDirection,
}

impl SeverityEventFilter {
    /// Returns whether `event` matches every configured condition.
    ///
    /// This is a direct port of the Go `eventFilterMatches` helper.
    pub fn matches(&self, event: SeverityEvent) -> bool {
        if !self.from_levels.is_empty() && !self.from_levels.contains(&event.from_level) {
            return false;
        }
        if !self.to_levels.is_empty() && !self.to_levels.contains(&event.to_level) {
            return false;
        }
        match self.direction {
            SeverityEventDirection::Escalation => {
                if event.to_level <= event.from_level {
                    return false;
                }
            }
            SeverityEventDirection::Deescalation => {
                if event.to_level >= event.from_level {
                    return false;
                }
            }
            SeverityEventDirection::Both => {}
        }
        true
    }
}

/// Configuration for one severity-event subscription's filter and cooldown.
///
/// The listener is passed separately when subscribing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeverityEventsConfiguration {
    /// Controls which transitions are delivered. The default filter delivers all transitions.
    pub filter: SeverityEventFilter,
    /// The minimum number of seconds that must elapse after a delivered transition before a downward
    /// (de-escalation) transition can be delivered again.
    ///
    /// Zero means no cooldown: every matching transition is delivered. The testbench scorer profile uses
    /// `0`; production uses `300`.
    pub cooldown_secs: i64,
}

/// Receives severity state-machine transitions from a dispatcher.
///
/// The dispatcher calls [`SeverityEventListener::on_severity_transition`] synchronously while advancing,
/// matching the Go callback. Because the Rust scorer is driven through a single `&mut` owner, a listener
/// cannot re-enter the scorer during delivery; record the event and read the scorer afterwards instead.
pub trait SeverityEventListener {
    /// Handles one delivered severity transition.
    fn on_severity_transition(&mut self, event: SeverityEvent);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(from: SeverityLevel, to: SeverityLevel) -> SeverityEvent {
        SeverityEvent {
            timestamp_sec: 1000,
            from_level: from,
            to_level: to,
            direction: event_direction(from, to),
        }
    }

    #[test]
    fn level_labels_match_go() {
        assert_eq!(SeverityLevel::Low.as_str(), "low");
        assert_eq!(SeverityLevel::Medium.as_str(), "medium");
        assert_eq!(SeverityLevel::High.as_str(), "high");
    }

    #[test]
    fn default_filter_matches_every_transition() {
        let filter = SeverityEventFilter::default();
        for (from, to) in [
            (SeverityLevel::Low, SeverityLevel::Medium),
            (SeverityLevel::Medium, SeverityLevel::High),
            (SeverityLevel::High, SeverityLevel::Low),
        ] {
            assert!(filter.matches(event(from, to)));
        }
    }

    #[test]
    fn filter_direction_rejects_opposite_transitions() {
        let escalations = SeverityEventFilter {
            direction: SeverityEventDirection::Escalation,
            ..Default::default()
        };
        let deescalations = SeverityEventFilter {
            direction: SeverityEventDirection::Deescalation,
            ..Default::default()
        };

        assert!(escalations.matches(event(SeverityLevel::Low, SeverityLevel::High)));
        assert!(!escalations.matches(event(SeverityLevel::High, SeverityLevel::Medium)));
        assert!(deescalations.matches(event(SeverityLevel::High, SeverityLevel::Medium)));
        assert!(!deescalations.matches(event(SeverityLevel::Low, SeverityLevel::High)));
    }

    #[test]
    fn filter_level_sets_are_anded() {
        let filter = SeverityEventFilter {
            from_levels: vec![SeverityLevel::Low],
            to_levels: vec![SeverityLevel::High, SeverityLevel::Medium],
            direction: SeverityEventDirection::Both,
        };
        assert!(filter.matches(event(SeverityLevel::Low, SeverityLevel::High)));
        assert!(!filter.matches(event(SeverityLevel::Medium, SeverityLevel::High)));
        assert!(!filter.matches(event(SeverityLevel::Low, SeverityLevel::Low)));
    }
}
