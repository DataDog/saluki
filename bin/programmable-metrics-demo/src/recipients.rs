//! The Recipients panel's record: the recipients of ADP's delivery hosts, as its status lists them, each kept for a
//! while after ADP stops listing it.
//!
//! ADP's privileged status route lists the recipients its resource table holds (see [`RecipientEntry`]), and recently
//! retired ones with their drain reports. The record follows each incarnation of each recipient across statuses: when
//! its state changed, and, once ADP no longer lists it, that it is gone (`retired` if it was draining), with the last
//! report ADP gave, so that a drain's outcome stays visible whether or not ADP keeps listing retired recipients.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::stub::protocol::{DrainReport, RecipientEntry};

/// How long a recipient ADP no longer lists stays in the record, in milliseconds.
pub const RETAINED_MS: u64 = 600_000;

/// How many recipients the record keeps at most; the ones gone longest are forgotten first.
pub const MAX_RECIPIENTS: usize = 64;

/// The state the panel shows for a state ADP reports: `live` reads `active`, every other state as reported.
fn shown_state(reported: &str) -> String {
    match reported {
        "live" => "active".to_string(),
        other => other.to_string(),
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RecipientKey {
    host: String,
    id: String,
    incarnation: Option<u64>,
}

#[derive(Clone, Debug)]
struct Tracked {
    entry: RecipientEntry,
    state: String,
    state_since_ms: u64,
    last_seen_ms: u64,
    listed: bool,
}

/// One recipient, as the panel shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecipientView {
    /// The delivery host.
    pub host: String,

    /// The recipient's identity.
    pub id: String,

    /// Whether a resources artifact declared it, rather than ADP's configuration.
    pub declared: bool,

    /// Its slot in its host's table.
    pub slot: Option<u64>,

    /// Its incarnation.
    pub incarnation: Option<u64>,

    /// The state shown: `active`, `draining`, `retired`, or another state ADP reports (`preparing`, `failed`,
    /// `exiting`); `gone` for one ADP stopped listing while it was not draining.
    pub state: String,

    /// When the record first saw it in its current state, in milliseconds since the Unix epoch.
    pub state_since_ms: u64,

    /// Whether ADP's latest status lists it.
    pub listed: bool,

    /// When ADP's status last listed it.
    pub last_seen_ms: u64,

    /// Who declared it, as ADP writes it.
    pub provenance: Option<Value>,

    /// The bytes its retry queue held, when ADP last listed it.
    pub queue_bytes: Option<u64>,

    /// The report of its last drain, if ADP gave one.
    pub last_drain: Option<DrainReport>,
}

/// What the panel shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecipientsView {
    /// Whether ADP's status lists recipients at all (from stage 7).
    pub reported: bool,

    /// Every recipient in the record, by host, identity, and incarnation.
    pub recipients: Vec<RecipientView>,
}

/// The record of the recipients ADP's status listed.
#[derive(Clone, Debug, Default)]
pub struct RecipientLog {
    reported: bool,
    recipients: BTreeMap<RecipientKey, Tracked>,
}

impl RecipientLog {
    /// Records the recipients a status received at `now_ms` lists: `None` when the status has no `recipients` member,
    /// which leaves the record as it is.
    pub fn observe(&mut self, listed: Option<&[RecipientEntry]>, now_ms: u64) {
        let Some(listed) = listed else {
            self.reported = false;
            return;
        };
        self.reported = true;
        let mut seen = Vec::with_capacity(listed.len());
        for entry in listed {
            let key = RecipientKey {
                host: entry.host.clone(),
                id: entry.id.clone(),
                incarnation: entry.incarnation,
            };
            let state = shown_state(&entry.state);
            let tracked = self.recipients.entry(key.clone()).or_insert_with(|| Tracked {
                entry: entry.clone(),
                state: state.clone(),
                state_since_ms: now_ms,
                last_seen_ms: now_ms,
                listed: true,
            });
            // A recipient listed live again after it retired or went (one ADP reports without incarnations) starts a
            // new life, whose drain is still to come.
            let revived = matches!(tracked.state.as_str(), "retired" | "gone") && !matches!(state.as_str(), "retired");
            if tracked.state != state || !tracked.listed {
                tracked.state = state;
                tracked.state_since_ms = now_ms;
            }
            // A report ADP gave once stays, should a later status leave it out, for as long as the life it reports on.
            let last_drain = if revived {
                entry.last_drain
            } else {
                entry.last_drain.or(tracked.entry.last_drain)
            };
            tracked.entry = RecipientEntry {
                last_drain,
                ..entry.clone()
            };
            tracked.last_seen_ms = now_ms;
            tracked.listed = true;
            seen.push(key);
        }
        for (key, tracked) in &mut self.recipients {
            if tracked.listed && !seen.contains(key) {
                tracked.listed = false;
                let state = match tracked.state.as_str() {
                    "draining" | "retired" => "retired",
                    _ => "gone",
                };
                if tracked.state != state {
                    tracked.state = state.to_string();
                    tracked.state_since_ms = now_ms;
                }
            }
        }
        self.recipients
            .retain(|_, tracked| tracked.listed || tracked.last_seen_ms + RETAINED_MS >= now_ms);
        while self.recipients.len() > MAX_RECIPIENTS {
            let oldest = self
                .recipients
                .iter()
                .filter(|(_, tracked)| !tracked.listed)
                .min_by_key(|(_, tracked)| tracked.last_seen_ms)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => self.recipients.remove(&key),
                None => break,
            };
        }
    }

    /// Returns what the panel shows.
    pub fn view(&self) -> RecipientsView {
        RecipientsView {
            reported: self.reported,
            recipients: self
                .recipients
                .values()
                .map(|tracked| RecipientView {
                    host: tracked.entry.host.clone(),
                    id: tracked.entry.id.clone(),
                    declared: tracked.entry.id.contains("/declared/"),
                    slot: tracked.entry.slot,
                    incarnation: tracked.entry.incarnation,
                    state: tracked.state.clone(),
                    state_since_ms: tracked.state_since_ms,
                    listed: tracked.listed,
                    last_seen_ms: tracked.last_seen_ms,
                    provenance: tracked.entry.provenance.clone(),
                    queue_bytes: tracked.entry.queue_bytes,
                    last_drain: tracked.entry.last_drain,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM_D: &str = "dd_out/declared/team_d/http://127.0.0.1:17105";

    fn entry(id: &str, incarnation: u64, state: &str) -> RecipientEntry {
        RecipientEntry {
            host: "dd_out".to_string(),
            id: id.to_string(),
            slot: Some(1),
            incarnation: Some(incarnation),
            state: state.to_string(),
            ..RecipientEntry::default()
        }
    }

    fn states(log: &RecipientLog) -> Vec<(String, Option<u64>, String, u64, bool)> {
        log.view()
            .recipients
            .into_iter()
            .map(|view| (view.id, view.incarnation, view.state, view.state_since_ms, view.listed))
            .collect()
    }

    #[test]
    fn a_recipient_that_drains_and_leaves_the_status_is_shown_retired_with_its_last_report() {
        let mut log = RecipientLog::default();
        log.observe(None, 1_000);
        assert_eq!(log.view(), RecipientsView::default());

        let primary = entry("dd_out/primary/0", 1, "live");
        log.observe(Some(&[primary.clone(), entry(TEAM_D, 1, "live")]), 2_000);
        log.observe(Some(&[primary.clone(), entry(TEAM_D, 1, "draining")]), 3_000);
        let mut retired = entry(TEAM_D, 1, "retired");
        retired.last_drain = Some(DrainReport {
            delivered: Some(4),
            dropped: Some(2),
            deadline_reached: Some(true),
            ..DrainReport::default()
        });
        log.observe(Some(&[primary.clone(), retired]), 4_000);
        // ADP stops listing it, and the report it gave stays.
        log.observe(Some(std::slice::from_ref(&primary)), 5_000);
        assert!(log.view().reported);
        assert_eq!(
            states(&log),
            [
                (
                    "dd_out/declared/team_d/http://127.0.0.1:17105".to_string(),
                    Some(1),
                    "retired".to_string(),
                    4_000,
                    false
                ),
                (
                    "dd_out/primary/0".to_string(),
                    Some(1),
                    "active".to_string(),
                    2_000,
                    true
                ),
            ]
        );
        let team_d = &log.view().recipients[0];
        assert!(team_d.declared);
        assert_eq!(team_d.last_drain.unwrap().dropped, Some(2));

        // A new incarnation of the same identity is a recipient of its own.
        log.observe(Some(&[primary.clone(), entry(TEAM_D, 2, "live")]), 6_000);
        assert_eq!(
            states(&log)[..2]
                .iter()
                .map(|(_, incarnation, state, _, _)| (*incarnation, state.as_str()))
                .collect::<Vec<_>>(),
            [(Some(1), "retired"), (Some(2), "active")]
        );

        // One that leaves the status while it is not draining is gone, and one forgotten after RETAINED_MS.
        log.observe(Some(&[]), 7_000);
        assert_eq!(
            states(&log)
                .iter()
                .map(|(_, _, state, _, _)| state.as_str())
                .collect::<Vec<_>>(),
            ["retired", "gone", "gone"]
        );
        log.observe(Some(&[]), 7_000 + RETAINED_MS + 1);
        assert!(log.view().recipients.is_empty());
    }

    #[test]
    fn a_recipient_listed_again_after_it_retired_forgets_the_drain_report_of_its_earlier_life() {
        // ADP lists no incarnations here: one identity is one entry of the record across its lives.
        let without_incarnation = |state: &str, dropped: Option<u64>| RecipientEntry {
            incarnation: None,
            last_drain: dropped.map(|dropped| DrainReport {
                dropped: Some(dropped),
                ..DrainReport::default()
            }),
            ..entry(TEAM_D, 0, state)
        };
        let mut log = RecipientLog::default();
        log.observe(Some(&[without_incarnation("retired", Some(3))]), 1_000);
        log.observe(Some(&[without_incarnation("live", None)]), 2_000);
        let view = log.view();
        assert_eq!(
            (view.recipients[0].state.as_str(), view.recipients[0].last_drain),
            ("active", None)
        );
        // A status that leaves the report out keeps the one this life got.
        log.observe(Some(&[without_incarnation("retired", Some(5))]), 3_000);
        log.observe(Some(&[without_incarnation("retired", None)]), 4_000);
        let view = log.view();
        assert_eq!(view.recipients[0].last_drain.unwrap().dropped, Some(5));
        assert_eq!(view.recipients[0].state_since_ms, 3_000);
    }

    #[test]
    fn the_record_keeps_a_bounded_number_of_recipients_that_left() {
        let mut log = RecipientLog::default();
        for index in 0..(MAX_RECIPIENTS as u64 + 10) {
            let id = format!("dd_out/declared/team_{}/http://127.0.0.1:1", index);
            log.observe(Some(&[entry(&id, 1, "live")]), index);
        }
        let view = log.view();
        assert_eq!(view.recipients.len(), MAX_RECIPIENTS);
        // The one listed last is kept, listed.
        assert!(view.recipients.iter().any(|recipient| recipient.listed));
        assert!(!view
            .recipients
            .iter()
            .any(|recipient| recipient.id == "dd_out/declared/team_0/http://127.0.0.1:1"));
    }
}
