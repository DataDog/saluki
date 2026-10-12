//! The installer core's waits, which share the caller's one retry timer.
//!
//! The caller keeps one timer: each [`Action::RetryAfter`][super::Action::RetryAfter] replaces the one set before, and
//! [`InstallerCore::retry`][super::InstallerCore::retry] is called once it passes. Several things wait on it at once,
//! each for its own delay: a candidate that failed to prepare, a cut whose arming was put off, a resource transaction
//! that left something out, and each failed recipient's next incarnation.
//!
//! The core has no clock, so it counts what passed by the delays it set the timer to. When the timer passes, every
//! wait that was waiting when it was set has waited that long. A new wait shorter than the delay the timer was set to
//! sets it again, and the waits already counting lose the time that passed since it was set; a new wait no shorter
//! starts counting when the timer is set next. So every wait lasts at least its delay, and a wait no other wait
//! interrupts lasts exactly its delay.

use std::{collections::BTreeMap, time::Duration};

use crate::ir::{HostId, RecipientId};

/// Something that waits for a delay.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Wait {
    /// The candidate that failed to prepare, before it is tried again.
    Candidate,

    /// The cut whose arming was put off, before it is armed again.
    Cut,

    /// The resource transaction that left an addition out, or whose host did not answer, before the next one.
    Transaction,

    /// A failed recipient, before its next incarnation is prepared.
    Recipient(HostId, RecipientId),
}

/// The delay still to wait of one wait.
#[derive(Clone, Copy, Debug)]
struct Remaining {
    delay: Duration,

    /// Whether the wait counts from when the timer was set last: it waited when the timer was set.
    counting: bool,
}

/// Every wait, and the delay the timer was set to.
#[derive(Debug, Default)]
pub(super) struct Waits {
    waits: BTreeMap<Wait, Remaining>,

    /// The delay the timer was set to last, while it has not passed.
    timer: Option<Duration>,
}

impl Waits {
    /// Starts `wait` for `delay`, replacing the wait it was in. Returns the delay to set the timer to, if it must be
    /// set: when no timer runs, or when the wait is shorter than the delay the timer was set to.
    pub(super) fn start(&mut self, wait: Wait, delay: Duration) -> Option<Duration> {
        let reset = self.timer.is_none_or(|timer| delay < timer);
        self.waits.insert(wait, Remaining { delay, counting: false });
        reset.then(|| self.set(delay))
    }

    /// Ends `wait` before its delay passed, if it waits.
    pub(super) fn cancel(&mut self, wait: &Wait) {
        self.waits.remove(wait);
    }

    /// Returns `true` if `wait` waits.
    pub(super) fn waiting(&self, wait: &Wait) -> bool {
        self.waits.contains_key(wait)
    }

    /// Handles the timer passing. Returns the waits whose delays passed, and the delay to set the timer to for the
    /// others, if any wait.
    ///
    /// A timer that passes while the core set none, such as one a caller set before a restart of the core, ends no
    /// wait.
    pub(super) fn passed(&mut self) -> (Vec<Wait>, Option<Duration>) {
        let Some(passed) = self.timer.take() else {
            return (Vec::new(), None);
        };
        let mut due = Vec::new();
        self.waits.retain(|wait, remaining| {
            if remaining.counting {
                remaining.delay = remaining.delay.saturating_sub(passed);
            }
            let ended = remaining.delay.is_zero();
            if ended {
                due.push(wait.clone());
            }
            !ended
        });
        let next = self.waits.values().map(|remaining| remaining.delay).min();
        (due, next.map(|delay| self.set(delay)))
    }

    /// Sets the timer to `delay`: every wait counts from now. Returns the delay.
    fn set(&mut self, delay: Duration) -> Duration {
        self.timer = Some(delay);
        for remaining in self.waits.values_mut() {
            remaining.counting = true;
        }
        delay
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn recipient(name: &str) -> Wait {
        Wait::Recipient(
            HostId::new("dd_out").unwrap(),
            RecipientId::new(format!("dd_out/additional/0/{}", name)).unwrap(),
        )
    }

    #[test]
    fn a_wait_alone_sets_the_timer_to_its_delay_and_ends_when_it_passes() {
        let mut waits = Waits::default();
        assert_eq!(waits.start(Wait::Candidate, secs(8)), Some(secs(8)));
        assert!(waits.waiting(&Wait::Candidate));
        assert_eq!(waits.passed(), (vec![Wait::Candidate], None));
        assert!(!waits.waiting(&Wait::Candidate));
        // A timer that passes while none is set ends nothing.
        assert_eq!(waits.passed(), (Vec::new(), None));
    }

    #[test]
    fn a_shorter_wait_sets_the_timer_again_without_ending_a_longer_one_early() {
        let mut waits = Waits::default();
        assert_eq!(waits.start(Wait::Candidate, secs(30)), Some(secs(30)));
        // A recipient's first failure waits 1 s, not the candidate's 30 s.
        assert_eq!(waits.start(recipient("a"), secs(1)), Some(secs(1)));
        assert_eq!(waits.passed(), (vec![recipient("a")], Some(secs(29))));
        assert!(waits.waiting(&Wait::Candidate));
        assert_eq!(waits.passed(), (vec![Wait::Candidate], None));
    }

    #[test]
    fn a_longer_wait_counts_from_the_next_setting_and_each_wait_keeps_its_own_delay() {
        let mut waits = Waits::default();
        assert_eq!(waits.start(recipient("a"), secs(1)), Some(secs(1)));
        // Another recipient that failed four times waits its own 8 s, from when the timer is set next.
        assert_eq!(waits.start(recipient("b"), secs(8)), None);
        assert_eq!(waits.passed(), (vec![recipient("a")], Some(secs(8))));
        assert_eq!(waits.start(recipient("a"), secs(2)), Some(secs(2)));
        assert_eq!(waits.passed(), (vec![recipient("a")], Some(secs(6))));
        assert_eq!(waits.passed(), (vec![recipient("b")], None));
    }

    #[test]
    fn a_cancelled_wait_never_ends() {
        let mut waits = Waits::default();
        waits.start(Wait::Candidate, secs(2));
        waits.start(Wait::Cut, secs(1));
        waits.cancel(&Wait::Candidate);
        assert_eq!(waits.passed(), (vec![Wait::Cut], None));
    }

    /// A caller around the waits: a clock, and one timer that each delay the waits return replaces.
    struct Caller {
        waits: Waits,
        now: Duration,
        deadline: Option<Duration>,
        /// When each wait started, and its delay.
        started: BTreeMap<Wait, (Duration, Duration)>,
    }

    impl Caller {
        fn set(&mut self, delay: Option<Duration>) {
            if let Some(delay) = delay {
                self.deadline = Some(self.now + delay);
            }
        }

        /// Lets the timer pass, and returns the waits that ended, each with how long it waited and its delay.
        fn pass(&mut self) -> Vec<(Wait, Duration, Duration)> {
            let Some(deadline) = self.deadline.take() else {
                return Vec::new();
            };
            self.now = deadline;
            let (due, next) = self.waits.passed();
            self.set(next);
            due.into_iter()
                .map(|wait| {
                    let (at, delay) = self.started.remove(&wait).expect("a wait that ends started");
                    (wait, self.now - at, delay)
                })
                .collect()
        }
    }

    proptest! {
        /// Whatever waits start or are cancelled, and however much time passes between events, the timer is set while
        /// anything waits, no wait ends before its delay passed, and every wait ends once the timer keeps passing.
        #[test]
        fn property_test_no_wait_ends_before_its_delay_and_every_wait_ends(
            events in prop::collection::vec((0u8..4, 0u8..3, 1u64..40, 0u64..20), 1..60),
        ) {
            let mut caller = Caller {
                waits: Waits::default(),
                now: Duration::ZERO,
                deadline: None,
                started: BTreeMap::new(),
            };
            let mut ended = Vec::new();
            for (kind, which, delay, gap) in events {
                // Time passes; the timer passes first if its deadline comes before the next event.
                let next = caller.now + secs(gap);
                if caller.deadline.is_some_and(|deadline| deadline <= next) {
                    ended.extend(caller.pass());
                } else {
                    caller.now = next;
                }
                let wait = match which {
                    0 => Wait::Candidate,
                    1 => recipient("a"),
                    _ => recipient("b"),
                };
                match kind {
                    0 | 1 => {
                        let set = caller.waits.start(wait.clone(), secs(delay));
                        caller.set(set);
                        caller.started.insert(wait, (caller.now, secs(delay)));
                    }
                    2 => {
                        caller.waits.cancel(&wait);
                        caller.started.remove(&wait);
                    }
                    _ => ended.extend(caller.pass()),
                }
                prop_assert!(caller.started.is_empty() || caller.deadline.is_some(), "a wait without a timer");
            }
            for _ in 0..8 {
                ended.extend(caller.pass());
            }
            prop_assert!(caller.started.is_empty(), "waits never ended: {:?}", caller.started);
            for (wait, waited, delay) in ended {
                prop_assert!(waited >= delay, "{:?} ended after {:?} of its {:?}", wait, waited, delay);
            }
        }
    }
}
