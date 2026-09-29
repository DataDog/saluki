//! Exercises the `test-util` surface as a subscriber's own tests would, through the public API alone.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use datadog_agent_remote_config::{decode_json, ApplyError, ConfigId, JsonError, ProductDecoder, TestPublisher};
use serde::Deserialize;
use tokio::time::timeout;

#[derive(Debug, Deserialize, PartialEq)]
struct Rule {
    service: String,
    rate: f64,
}

/// Every assigned sampling rule, in ascending configuration ID order.
#[derive(Debug, PartialEq)]
struct Rules(Vec<Rule>);

#[derive(Debug)]
enum RulesError {
    Malformed(JsonError),
    OutOfRange { service: String },
    DuplicateService { service: String },
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(error) => error.fmt(f),
            Self::OutOfRange { service } => write!(f, "The rate for {service} is not between 0 and 1."),
            Self::DuplicateService { service } => write!(f, "Several rules name {service}."),
        }
    }
}

impl ApplyError for RulesError {
    fn apply_error(&self) -> String {
        self.to_string()
    }
}

#[derive(Default)]
struct RulesDecoder {
    rules: Vec<Rule>,
}

impl ProductDecoder for RulesDecoder {
    const PRODUCT: &'static str = "EXAMPLE_SAMPLING_RULES";

    type Snapshot = Rules;
    type Error = RulesError;

    fn decode(&mut self, _id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        let rule: Rule = decode_json(payload).map_err(RulesError::Malformed)?;
        if !(0.0..=1.0).contains(&rule.rate) {
            return Err(RulesError::OutOfRange { service: rule.service });
        }
        self.rules.push(rule);
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        for (index, rule) in self.rules.iter().enumerate() {
            if self.rules[..index]
                .iter()
                .any(|earlier| earlier.service == rule.service)
            {
                return Err(RulesError::DuplicateService {
                    service: rule.service.clone(),
                });
            }
        }
        Ok(Rules(self.rules))
    }
}

fn rule(service: &str, rate: f64) -> Rule {
    Rule {
        service: service.to_owned(),
        rate,
    }
}

#[tokio::test]
async fn assign_decodes_in_ascending_order_and_skips_rejected_configurations() {
    let (publisher, mut subscription) = TestPublisher::<Rules, RulesError>::new();
    assert!(subscription.current().is_none());

    publisher.assign::<RulesDecoder>([
        ("rules.c", r#"{"service": "cart", "rate": 0.5}"#),
        ("rules.a", r#"{"service": "api", "rate": 1.0}"#),
        ("rules.b", r#"{"service": "billing", "rate": 2.0}"#),
        ("rules.d", r#"{"service": "db""#),
    ]);

    let snapshot = subscription.changed().await.unwrap();
    assert_eq!(*snapshot, Rules(vec![rule("api", 1.0), rule("cart", 0.5)]));
    assert!(Arc::ptr_eq(&snapshot, &subscription.current().unwrap()));
}

#[tokio::test]
async fn a_rejection_keeps_the_last_accepted_snapshot() {
    let (publisher, mut subscription) = TestPublisher::<Rules, RulesError>::new();
    publisher.assign::<RulesDecoder>([("rules.a", r#"{"service": "api", "rate": 1.0}"#)]);
    let accepted = subscription.changed().await.unwrap();

    // A build failure from `assign` is delivered as the subscriber's own error.
    publisher.assign::<RulesDecoder>([
        ("rules.a", r#"{"service": "api", "rate": 1.0}"#),
        ("rules.b", r#"{"service": "api", "rate": 0.1}"#),
    ]);
    let error = subscription.changed().await.unwrap_err();
    assert!(matches!(&*error, RulesError::DuplicateService { service } if service == "api"));
    assert_eq!(error.apply_error(), "Several rules name api.");
    assert!(Arc::ptr_eq(&accepted, &subscription.current().unwrap()));

    // So is a finished rejection.
    publisher.reject(RulesError::OutOfRange {
        service: "api".to_owned(),
    });
    assert!(matches!(
        &*subscription.changed().await.unwrap_err(),
        RulesError::OutOfRange { .. }
    ));
    assert!(Arc::ptr_eq(&accepted, &subscription.current().unwrap()));

    // The next accepted snapshot replaces it.
    publisher.accept(Rules(Vec::new()));
    assert_eq!(*subscription.changed().await.unwrap(), Rules(Vec::new()));
    assert_eq!(*subscription.current().unwrap(), Rules(Vec::new()));
}

#[tokio::test]
async fn a_clone_made_after_a_publish_is_observed_reads_it_from_current() {
    let (publisher, mut subscription) = TestPublisher::<Rules, RulesError>::new();
    publisher.accept(Rules(vec![rule("api", 1.0)]));
    subscription.changed().await.unwrap();

    // The clone starts where its source is, so the publish is already observed and only `current` shows it.
    let mut late = subscription.clone();
    assert_eq!(*late.current().unwrap(), Rules(vec![rule("api", 1.0)]));
    assert!(timeout(Duration::from_millis(10), late.changed()).await.is_err());

    publisher.accept(Rules(Vec::new()));
    assert_eq!(*late.changed().await.unwrap(), Rules(Vec::new()));
}

#[tokio::test]
async fn a_dropped_publisher_leaves_changed_pending_after_the_last_publication() {
    let (publisher, mut subscription) = TestPublisher::<Rules, RulesError>::new();
    publisher.assign::<RulesDecoder>([("rules.a", r#"{"service": "api", "rate": 1.0}"#)]);
    drop(publisher);

    assert_eq!(*subscription.changed().await.unwrap(), Rules(vec![rule("api", 1.0)]));
    assert!(timeout(Duration::from_millis(10), subscription.changed())
        .await
        .is_err());
    assert_eq!(*subscription.current().unwrap(), Rules(vec![rule("api", 1.0)]));
}
