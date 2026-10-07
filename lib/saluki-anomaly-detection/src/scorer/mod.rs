//! The anomaly scorer: EWMA severity scoring, severity-event dispatch, and episodes.
//!
//! This module ports the Go anomaly scorer (`observer/impl/anomaly_scorer.go`) and its severity-event
//! helpers (`severityevents/def`, `severityevents/impl/dispatcher.go`):
//!
//! * [`severity`] defines the severity vocabulary: levels, transition events, filters, and the listener
//!   trait.
//! * [`Dispatcher`] is the per-subscription filter/cooldown state machine.
//! * [`AnomalyScorer`] is the unified scoring pipeline: it buffers accepted anomalies, deduplicates
//!   them per series over a sliding window, computes a saturated EWMA per second, derives raw and
//!   delivered severity, and tracks severity episodes with bounded contributor attribution.
//!
//! The scorer implements the [`crate::traits::Scorer`] trait; its episode lifecycle events are the
//! scorer outputs. See the [`anomaly_scorer`] module documentation for the exact lifecycle and the
//! ordering guarantees inside a multi-second advance.
//!
//! Two production-only concerns of the Go scorer are deliberately not ported: telemetry gauges (the
//! internal watcher's per-tick gauge writes) and the optional transition logging behind
//! [`crate::config::AnomalyScorerConfig::logs`]. The configuration field is kept for settings parity, but
//! the Rust crate is a dependency-free analytical core and emits no telemetry or logs; consumers observe
//! transitions through subscriptions and the score-tick hook instead.

mod anomaly_scorer;
pub mod dispatcher;
pub mod severity;
mod top_anomaly;

pub use anomaly_scorer::{
    anomaly_severity_label, ActiveCorrelation, AnomalyScoreBucket, AnomalyScoreState, AnomalyScorer, CorrelatorEvent,
    CorrelatorEventKind, ScoreTick, ScoreTickHook, ScorerContributor, LEVEL_WEIGHTS,
};
pub use dispatcher::Dispatcher;
pub use severity::{
    event_direction, SeverityEvent, SeverityEventDirection, SeverityEventFilter, SeverityEventListener,
    SeverityEventsConfiguration, SeverityLevel,
};
