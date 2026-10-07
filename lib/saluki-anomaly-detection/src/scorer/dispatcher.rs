//! The per-subscription severity delivery state machine.
//!
//! This is a port of the Go `comp/anomalydetection/severityevents/impl/dispatcher.go`. A dispatcher owns
//! one listener plus one fixed filter/cooldown state machine: it receives the scorer's raw per-second
//! severity level and decides whether a transition event is delivered to the listener.
//!
//! The Go dispatcher invokes its listener and returns nothing. The Rust port both invokes an attached
//! listener **and** returns the delivered event, because the scorer's internal episode watcher cannot be
//! a listener of the scorer it belongs to (that would require a self-referential callback): instead, the
//! watcher's dispatcher has no listener and the scorer applies its episode logic to the returned event.

use crate::scorer::severity::{
    event_direction, SeverityEvent, SeverityEventDirection, SeverityEventListener, SeverityEventsConfiguration,
    SeverityLevel,
};

/// Delivers filtered, cooldown-adjusted severity transitions to one listener.
///
/// Cooldown suppresses downward (de-escalation) transitions for `cooldown_secs` seconds after the last
/// **delivered** transition; escalations are never suppressed.
pub struct Dispatcher {
    listener: Option<Box<dyn SeverityEventListener>>,
    filter: crate::scorer::severity::SeverityEventFilter,
    cooldown_secs: i64,

    level: SeverityLevel,
    last_sec: i64,

    last_state_entry_ts: i64,
}

impl Dispatcher {
    /// Creates a dispatcher delivering to `listener`, filtered and cooled down per `cfg`.
    ///
    /// A `None` listener produces a pure state machine whose events are only visible through the return
    /// values of [`Dispatcher::deliver_initial`] and [`Dispatcher::advance`]; the scorer uses this mode for
    /// its internal episode watcher.
    pub fn new(cfg: SeverityEventsConfiguration, listener: Option<Box<dyn SeverityEventListener>>) -> Self {
        Self {
            listener,
            filter: cfg.filter,
            cooldown_secs: cfg.cooldown_secs,
            level: SeverityLevel::Low,
            last_sec: 0,
            last_state_entry_ts: 0,
        }
    }

    /// Seeds the dispatcher from the current level, delivering a synthetic snapshot event when the level
    /// is not Low.
    ///
    /// The synthetic event is an escalation from Low to `level` at `sec`; it starts the cooldown only when
    /// it passes the filter and is delivered, exactly as the Go `DeliverInitial` does.
    pub fn deliver_initial(&mut self, sec: i64, level: SeverityLevel) -> Option<SeverityEvent> {
        self.level = level;
        self.last_sec = sec;
        if level == SeverityLevel::Low {
            return None;
        }

        let event = SeverityEvent {
            timestamp_sec: sec,
            from_level: SeverityLevel::Low,
            to_level: level,
            direction: SeverityEventDirection::Escalation,
        };
        if self.filter.matches(event) {
            self.last_state_entry_ts = sec;
            self.deliver(event);
            Some(event)
        } else {
            None
        }
    }

    /// Feeds one raw severity level into the dispatcher state machine.
    ///
    /// Returns the transition event that was delivered to the listener (if any); an unchanged level, a
    /// cooldown-suppressed de-escalation, or a filtered-out transition all return `None`.
    pub fn advance(&mut self, sec: i64, level: SeverityLevel) -> Option<SeverityEvent> {
        let next = level;
        if next == self.level {
            self.last_sec = sec;
            return None;
        }

        if next < self.level && self.cooldown_secs > 0 && sec - self.last_state_entry_ts < self.cooldown_secs {
            self.last_sec = sec;
            return None;
        }

        let event = SeverityEvent {
            timestamp_sec: sec,
            from_level: self.level,
            to_level: next,
            direction: event_direction(self.level, next),
        };
        self.level = next;
        self.last_sec = sec;
        self.last_state_entry_ts = sec;

        if self.filter.matches(event) {
            self.deliver(event);
            Some(event)
        } else {
            None
        }
    }

    /// Clears delivery state and the known level, restoring the fresh-subscriber defaults.
    pub fn reset(&mut self) {
        self.level = SeverityLevel::Low;
        self.last_sec = 0;
        self.last_state_entry_ts = 0;
    }

    /// Returns the currently delivered (post-filter, post-cooldown) level.
    pub fn level(&self) -> SeverityLevel {
        self.level
    }

    /// Returns the second of the most recent [`Dispatcher::advance`] call.
    pub fn last_sec(&self) -> i64 {
        self.last_sec
    }

    fn deliver(&mut self, event: SeverityEvent) {
        if let Some(listener) = self.listener.as_mut() {
            listener.on_severity_transition(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scorer::severity::SeverityEventFilter;

    fn dispatcher(cooldown_secs: i64) -> Dispatcher {
        Dispatcher::new(
            SeverityEventsConfiguration {
                cooldown_secs,
                ..Default::default()
            },
            None,
        )
    }

    /// A collector shared with the dispatcher's boxed listener via `Rc<RefCell<...>>`.
    type SharedCollector = std::rc::Rc<std::cell::RefCell<Vec<SeverityEvent>>>;

    fn shared_collector_dispatcher(cfg: SeverityEventsConfiguration) -> (Dispatcher, SharedCollector) {
        let events: SharedCollector = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = events.clone();
        let listener = Box::new(move |event: SeverityEvent| sink.borrow_mut().push(event));
        // A closure is not a SeverityEventListener; wrap it in an adapter.
        let listener = Box::new(ClosureListener(listener));
        (Dispatcher::new(cfg, Some(listener)), events)
    }

    struct ClosureListener(Box<dyn FnMut(SeverityEvent)>);

    impl SeverityEventListener for ClosureListener {
        fn on_severity_transition(&mut self, event: SeverityEvent) {
            (self.0)(event);
        }
    }

    fn escalations_only() -> SeverityEventsConfiguration {
        SeverityEventsConfiguration {
            filter: SeverityEventFilter {
                direction: SeverityEventDirection::Escalation,
                ..Default::default()
            },
            cooldown_secs: 0,
        }
    }

    #[test]
    fn dispatcher_basic_delivers_escalation() {
        let mut d = dispatcher(0);
        assert_eq!(d.advance(1000, SeverityLevel::Low), None);
        let event = d.advance(1001, SeverityLevel::High).expect("escalation delivered");

        assert_eq!(event.from_level, SeverityLevel::Low);
        assert_eq!(event.to_level, SeverityLevel::High);
        assert_eq!(event.direction, SeverityEventDirection::Escalation);
        assert_eq!(event.timestamp_sec, 1001);
    }

    #[test]
    fn dispatcher_cooldown_blocks_then_releases_deescalation() {
        let mut d = dispatcher(60);

        d.advance(1000, SeverityLevel::Low);
        d.advance(1001, SeverityLevel::High); // escalation
        assert_eq!(
            d.advance(1002, SeverityLevel::Low),
            None,
            "de-escalation within cooldown"
        );
        assert_eq!(d.level(), SeverityLevel::High, "level stays High while suppressed");

        assert!(
            d.advance(1062, SeverityLevel::Low).is_some(),
            "de-escalation after cooldown expiry"
        );
    }

    #[test]
    fn dispatcher_cooldown_zero_delivers_every_transition() {
        let mut d = dispatcher(0);

        d.advance(1000, SeverityLevel::Low);
        assert!(d.advance(1001, SeverityLevel::High).is_some());
        // Cooldown 0: the de-escalation is delivered immediately on the next second.
        assert!(d.advance(1002, SeverityLevel::Low).is_some());
    }

    #[test]
    fn dispatcher_cooldown_three_hundred_blocks_long_windows() {
        let mut d = dispatcher(300);

        d.advance(1000, SeverityLevel::Low);
        assert!(d.advance(1001, SeverityLevel::High).is_some());
        assert_eq!(d.advance(1100, SeverityLevel::Low), None, "still within 300s cooldown");
        assert_eq!(
            d.advance(1300, SeverityLevel::Low),
            None,
            "boundary second still blocked"
        );
        assert!(
            d.advance(1301, SeverityLevel::Low).is_some(),
            "one second past the cooldown boundary is delivered"
        );
    }

    #[test]
    fn dispatcher_filter_suppresses_non_matching_transitions() {
        let mut d = Dispatcher::new(escalations_only(), None);

        d.advance(1000, SeverityLevel::Low);
        assert!(d.advance(1001, SeverityLevel::High).is_some(), "escalation passes");
        assert_eq!(d.advance(1002, SeverityLevel::Low), None, "de-escalation filtered out");
        // The state machine still moved to Low even though the event was filtered.
        assert_eq!(d.level(), SeverityLevel::Low);
    }

    #[test]
    fn dispatcher_reset_clears_subscription_state() {
        let mut d = dispatcher(3600);

        d.advance(1000, SeverityLevel::Low);
        d.advance(1001, SeverityLevel::High);

        d.reset();
        // After reset the dispatcher starts at Low again, so the same level emits a fresh escalation.
        let event = d.advance(2001, SeverityLevel::High).expect("post-reset escalation");
        assert_eq!(event.from_level, SeverityLevel::Low);
        assert_eq!(d.level(), SeverityLevel::High);
    }

    #[test]
    fn deliver_initial_low_emits_nothing() {
        let mut d = dispatcher(0);
        assert_eq!(d.deliver_initial(1000, SeverityLevel::Low), None);
        assert_eq!(d.level(), SeverityLevel::Low);
    }

    #[test]
    fn default_dispatcher_emits_first_non_low_transition() {
        let mut d = dispatcher(0);
        let event = d.advance(1000, SeverityLevel::High).expect("first non-Low level emits");
        assert_eq!(event.from_level, SeverityLevel::Low);
        assert_eq!(event.to_level, SeverityLevel::High);
    }

    #[test]
    fn deliver_initial_bootstraps_from_low() {
        let mut d = dispatcher(0);
        let event = d.deliver_initial(1001, SeverityLevel::High).expect("initial event");

        assert_eq!(event.from_level, SeverityLevel::Low);
        assert_eq!(event.to_level, SeverityLevel::High);
        assert_eq!(event.direction, SeverityEventDirection::Escalation);
        assert_eq!(event.timestamp_sec, 1001);

        // An unchanged level on the next advance must not re-emit.
        assert_eq!(d.advance(1002, SeverityLevel::High), None);
    }

    #[test]
    fn deliver_initial_respects_direction_filter() {
        let mut escalations = Dispatcher::new(escalations_only(), None);
        assert!(escalations.deliver_initial(1001, SeverityLevel::High).is_some());

        let mut deescalations = Dispatcher::new(
            SeverityEventsConfiguration {
                filter: SeverityEventFilter {
                    direction: SeverityEventDirection::Deescalation,
                    ..Default::default()
                },
                cooldown_secs: 0,
            },
            None,
        );
        assert_eq!(deescalations.deliver_initial(1001, SeverityLevel::High), None);
    }

    #[test]
    fn deliver_initial_starts_cooldown_when_delivered() {
        let mut d = dispatcher(60);
        assert!(d.deliver_initial(1001, SeverityLevel::High).is_some());

        assert_eq!(
            d.advance(1010, SeverityLevel::Low),
            None,
            "cooldown blocks de-escalation"
        );
        assert!(d.advance(1062, SeverityLevel::Low).is_some(), "cooldown expired");
    }

    #[test]
    fn filtered_initial_event_does_not_start_cooldown() {
        let mut d = Dispatcher::new(
            SeverityEventsConfiguration {
                filter: SeverityEventFilter {
                    direction: SeverityEventDirection::Deescalation,
                    ..Default::default()
                },
                cooldown_secs: 60,
            },
            None,
        );
        assert_eq!(d.deliver_initial(1000, SeverityLevel::High), None);

        // The bootstrap escalation was filtered out, so it must not seed the cooldown.
        let event = d.advance(1005, SeverityLevel::Low).expect("immediate de-escalation");
        assert_eq!(event.direction, SeverityEventDirection::Deescalation);
    }

    #[test]
    fn reset_clears_known_level_for_new_subscribers() {
        let mut d = dispatcher(0);
        d.deliver_initial(1001, SeverityLevel::High);

        d.reset();
        assert!(
            d.advance(1002, SeverityLevel::High).is_some(),
            "reset restores the Low default"
        );
    }

    #[test]
    fn attached_listener_receives_delivered_events() {
        let (mut d, events) = shared_collector_dispatcher(SeverityEventsConfiguration::default());

        d.advance(1000, SeverityLevel::Low);
        d.advance(1001, SeverityLevel::High);
        d.advance(1002, SeverityLevel::Low);

        assert_eq!(events.borrow().len(), 2);
        assert_eq!(events.borrow()[0].to_level, SeverityLevel::High);
        assert_eq!(events.borrow()[1].to_level, SeverityLevel::Low);
    }

    #[test]
    fn attached_listener_honors_cooldown() {
        let (mut d, events) = shared_collector_dispatcher(SeverityEventsConfiguration {
            cooldown_secs: 60,
            ..Default::default()
        });

        d.advance(1000, SeverityLevel::Low);
        d.advance(1001, SeverityLevel::High);
        d.advance(1002, SeverityLevel::Low); // suppressed by cooldown
        d.advance(1062, SeverityLevel::Low); // delivered

        assert_eq!(events.borrow().len(), 2);
        assert_eq!(events.borrow()[1].direction, SeverityEventDirection::Deescalation);
    }
}
