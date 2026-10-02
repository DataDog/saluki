//! Loads semantic mappings from the `APM_SEMANTIC_CORE_DD` Remote Configuration product.
//!
//! The Remote Configuration client gives [`SemanticCoreDecoder`] the currently assigned files. It selects a complete
//! registry for [`SemanticRegistryProvider`](super::provider::SemanticRegistryProvider) to serve to trace consumers.
//! This follows the Datadog Agent's `onSemanticCoreUpdate` (`pkg/trace/remoteconfighandler/remote_config_handler.go`).

use std::sync::Arc;

use datadog_agent_remote_config::{ConfigId, ProductDecoder};
use tracing::{info, warn};

use super::provider::{Origin, SemanticCore, SemanticCoreError};
use super::registry::{Registry, EMBEDDED_REGISTRY};

/// Selects a registry from the configurations assigned to `APM_SEMANTIC_CORE_DD`.
///
/// The Remote Configuration client calls [`ProductDecoder::decode`] for each assigned file in ID order, then
/// [`ProductDecoder::build`] once. Each file is a complete replacement for the registry, not a patch. Unknown
/// concepts and unsupported fallbacks are skipped; the rest of a valid file is used.
///
/// With no files assigned (including after removal or expiration), the embedded registry is used. If all assigned
/// files are invalid, the update is rejected and readers keep the last accepted registry. When multiple files are
/// assigned, a warning is logged and the last valid file in ID order wins. Valid files are acknowledged only when
/// the update is accepted.
///
/// The Datadog Agent orders files by full path rather than ID, so it may choose a different file when multiple
/// are assigned.
#[derive(Default)]
pub(crate) struct SemanticCoreDecoder {
    assigned: usize,
    chosen: Option<(ConfigId, Registry)>,
}

impl ProductDecoder for SemanticCoreDecoder {
    const PRODUCT: &'static str = "APM_SEMANTIC_CORE_DD";

    type Snapshot = SemanticCore;
    type Error = SemanticCoreError;

    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        // Count before parsing so an all-invalid assignment is not mistaken for an empty one.
        self.assigned += 1;
        let (registry, skipped) = Registry::from_slice_permissive(payload)
            .map_err(|e| SemanticCoreError::InvalidRegistry { reason: e.to_string() })?;
        if !skipped.is_empty() {
            warn!(
                config_id = %id,
                %skipped,
                "APM_SEMANTIC_CORE_DD configuration parsed partially; skipped what the registry cannot represent."
            );
        }
        self.chosen = Some((id.clone(), registry));
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        let Self { assigned, chosen } = self;
        // Match the Datadog Agent's warning even when every assigned file is invalid.
        if assigned > 1 {
            warn!(
                assigned,
                chosen = chosen.as_ref().map(|(id, _)| id.to_string()),
                "APM_SEMANTIC_CORE_DD delivered more than one configuration; expected one. Using the last valid \
                 configuration, if any."
            );
        }
        let core = match chosen {
            Some((id, registry)) => SemanticCore {
                registry: Arc::new(registry),
                origin: Origin::Remote(id),
                assigned,
            },
            None if assigned == 0 => SemanticCore {
                registry: Arc::clone(&EMBEDDED_REGISTRY),
                origin: Origin::Embedded,
                assigned,
            },
            None => return Err(SemanticCoreError::NoValidConfiguration { assigned }),
        };
        info!(
            content_hash = core.registry.content_hash(),
            version = core.registry.version(),
            source = %core.origin,
            "Semantic registry selected."
        );
        Ok(core)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::{self, Write as _};
    use std::sync::Mutex;

    use datadog_agent_remote_config::{ApplyError as _, TestPublisher};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

    use super::*;
    use crate::common::otlp::semantics::{Concept, SemanticRegistryProvider};

    const DB_STATEMENT: &str = r#"{"db.statement":{"canonical":"db.statement","fallbacks":[{"name":"db.statement","provider":"datadog","type":"string"}]}}"#;
    const HTTP_METHOD: &str = r#"{"http.method":{"canonical":"http.method","fallbacks":[{"name":"http.method","provider":"otel","type":"string"}]}}"#;

    fn payload(version: &str, concepts: &str) -> String {
        format!(r#"{{"version":"{version}","metadata":{{"content_hash":"hash-{version}"}},"concepts":{concepts}}}"#)
    }

    const EMPTY_CONCEPTS: &str = r#"{"version":"x","metadata":{"content_hash":"hash-a"},"concepts":{}}"#;

    fn decode_all(assignment: &[(&str, &str)]) -> (Vec<Result<(), String>>, Result<SemanticCore, String>) {
        let mut decoder = SemanticCoreDecoder::default();
        let verdicts = assignment
            .iter()
            .map(|(id, payload)| {
                decoder
                    .decode(&ConfigId::new(*id), payload.as_bytes())
                    .map_err(|e| e.apply_error())
            })
            .collect();
        (verdicts, decoder.build().map_err(|e| e.apply_error()))
    }

    struct EventRecorder(Arc<Mutex<Vec<(Level, String)>>>);

    struct FieldWriter(String);

    impl Visit for FieldWriter {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            write!(self.0, "{}={:?}", field.name(), value).expect("writing to a `String` does not fail");
        }
    }

    impl<S: Subscriber> Layer<S> for EventRecorder {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = FieldWriter(String::new());
            event.record(&mut fields);
            let level = *event.metadata().level();
            self.0
                .lock()
                .expect("no test panics while recording")
                .push((level, fields.0));
        }
    }

    fn events_while(f: impl FnOnce()) -> Vec<(Level, String)> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(EventRecorder(Arc::clone(&events)));
        tracing::subscriber::with_default(subscriber, f);
        Arc::try_unwrap(events)
            .expect("the subscriber is dropped")
            .into_inner()
            .expect("no test panics while recording")
    }

    fn warning_fields_while(f: impl FnOnce()) -> Vec<String> {
        events_while(f)
            .into_iter()
            .filter(|(level, _)| *level == Level::WARN)
            .map(|(_, fields)| fields)
            .collect()
    }

    fn warnings_while(f: impl FnOnce()) -> usize {
        warning_fields_while(f).len()
    }

    #[test]
    fn empty_assignment_reuses_the_embedded_registry() {
        let snapshot = SemanticCoreDecoder::default()
            .build()
            .expect("an empty assignment should build");
        assert!(Arc::ptr_eq(&snapshot.registry, &EMBEDDED_REGISTRY));
        assert_eq!(snapshot.origin, Origin::Embedded);
        assert_eq!(snapshot.assigned, 0);
    }

    #[test]
    fn decode_rejects_an_invalid_payload_with_its_fixed_message() {
        let not_json = "not json: secret.token";
        let no_concepts = r#"{"version":"secret.token","metadata":{"content_hash":"secret.token"},"concepts":{}}"#;
        let (verdicts, _) = decode_all(&[("bad", not_json), ("empty", no_concepts)]);
        assert_eq!(
            verdicts,
            [
                Err("Registry JSON is malformed at line 1, column 2.".to_owned()),
                Err("Registry JSON contains no concepts.".to_owned()),
            ]
        );
    }

    // The Datadog Agent accepts unknown concept keys (`pkg/trace/semantics/registry.go:107-111`).
    #[test]
    fn decode_loads_known_concepts_and_warns_about_unknown_ones() {
        let mixed = payload(
            "mixed",
            r#"{"db.statement":{"fallbacks":[{"name":"db.statement","provider":"datadog","type":"string"}]},"future.concept":{"fallbacks":[]}}"#,
        );
        let mut outcome = None;
        let warnings = warning_fields_while(|| outcome = Some(decode_all(&[("a", &mixed)])));
        let (verdicts, snapshot) = outcome.expect("the closure ran");

        assert_eq!(verdicts, [Ok(())]);
        let snapshot = snapshot.expect("a partially loaded configuration should build");
        assert!(snapshot
            .registry
            .get_attribute_precedence(Concept::DbStatement)
            .is_some());
        assert_eq!(
            warnings,
            [
                r#"message=APM_SEMANTIC_CORE_DD configuration parsed partially; skipped what the registry cannot represent. config_id=a skipped=unknown concepts: 1 ("future.concept")"#
            ]
        );
    }

    #[test]
    fn decode_keeps_the_other_fallbacks_of_a_concept_with_an_unrepresentable_entry() {
        let partial = payload(
            "partial",
            r#"{"http.status_code":{"fallbacks":[{"name":"vendor.status","provider":"vendor","type":"int64"},{"name":"kept.status","provider":"otel","type":"int64"}]}}"#,
        );
        let mut outcome = None;
        let warnings = warning_fields_while(|| outcome = Some(decode_all(&[("a", &partial)])));
        let (_, snapshot) = outcome.expect("the closure ran");

        let snapshot = snapshot.expect("a partially loaded configuration should build");
        let names: Vec<&str> = snapshot
            .registry
            .get_attribute_precedence(Concept::HttpStatusCode)
            .expect("the concept should be kept")
            .iter()
            .map(|tag| tag.name.as_str())
            .collect();
        assert_eq!(names, ["kept.status"]);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].ends_with(r#"skipped=unrepresentable fallbacks: 1 ("http.status_code"[0])"#),
            "{warnings:?}"
        );
    }

    #[test]
    fn decode_does_not_warn_about_a_fully_known_payload() {
        let known = payload("known", DB_STATEMENT);
        assert_eq!(
            warning_fields_while(|| drop(decode_all(&[("a", &known)]))),
            Vec::<String>::new()
        );
    }

    // Ported from upstream `TestOnSemanticCoreUpdate_MultipleValidConfigs_LastWins` and
    // `TestOnSemanticCoreUpdate_MixedBatch`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler_test.go#L623-L659
    #[test]
    fn last_valid_configuration_wins() {
        let early = payload("early", DB_STATEMENT);
        let late = payload("late", HTTP_METHOD);
        let (verdicts, snapshot) = decode_all(&[("a", &early), ("b", &late), ("c", "malformed")]);

        assert_eq!(
            verdicts,
            [
                Ok(()),
                Ok(()),
                Err("Registry JSON is malformed at line 1, column 1.".to_owned())
            ]
        );
        let snapshot = snapshot.expect("a valid configuration should build");
        assert_eq!(snapshot.origin, Origin::Remote(ConfigId::new("b")));
        assert_eq!(snapshot.registry.version(), "late");
        assert!(snapshot
            .registry
            .get_attribute_precedence(Concept::HttpMethod)
            .is_some());
        assert!(
            snapshot
                .registry
                .get_attribute_precedence(Concept::DbStatement)
                .is_none(),
            "configurations must not be merged"
        );
    }

    #[test]
    fn decode_counts_configurations_that_fail_to_parse() {
        let valid = payload("v", DB_STATEMENT);
        let (_, snapshot) = decode_all(&[("a", "not json"), ("b", &valid)]);
        assert_eq!(snapshot.expect("a valid configuration should build").assigned, 2);
    }

    #[test]
    fn build_warns_only_when_more_than_one_configuration_is_assigned() {
        let valid = payload("v", DB_STATEMENT);
        assert_eq!(warnings_while(|| drop(decode_all(&[("a", &valid)]))), 0);
        assert_eq!(
            warnings_while(|| drop(decode_all(&[("a", "not json"), ("b", &valid)]))),
            1
        );
    }

    // The Datadog Agent also warns when every assigned file is invalid.
    #[test]
    fn build_warns_about_more_than_one_configuration_even_when_all_are_rejected() {
        assert_eq!(
            warnings_while(|| drop(decode_all(&[("a", "not json"), ("b", EMPTY_CONCEPTS)]))),
            1
        );
    }

    // Ported from upstream `TestOnSemanticCoreUpdate_AllErrors`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler_test.go#L661
    #[test]
    fn all_invalid_configurations_reject_the_snapshot() {
        let (_, snapshot) = decode_all(&[("a", "not json"), ("b", EMPTY_CONCEPTS)]);
        assert_eq!(
            snapshot.err(),
            Some("None of the 2 assigned APM_SEMANTIC_CORE_DD configurations is a valid registry.".to_owned())
        );
    }

    #[tokio::test]
    async fn assign_publishes_the_last_valid_configuration() {
        let (publisher, mut subscription) = TestPublisher::<SemanticCore, SemanticCoreError>::new();
        let early = payload("early", DB_STATEMENT);
        let late = payload("late", HTTP_METHOD);
        publisher.assign::<SemanticCoreDecoder>([("c", "malformed"), ("b", late.as_str()), ("a", early.as_str())]);

        let snapshot = subscription
            .changed()
            .await
            .expect("a valid configuration should be accepted");
        assert_eq!(snapshot.origin, Origin::Remote(ConfigId::new("b")));
        assert_eq!(snapshot.registry.version(), "late");
        assert_eq!(snapshot.assigned, 3);
    }

    fn provider() -> (TestPublisher<SemanticCore, SemanticCoreError>, SemanticRegistryProvider) {
        let (publisher, subscription) = TestPublisher::new();
        (publisher, SemanticRegistryProvider::from_subscription(subscription))
    }

    #[test]
    fn provider_returns_the_assigned_registry() {
        let (publisher, provider) = provider();
        publisher.assign::<SemanticCoreDecoder>([("a", payload("remote", DB_STATEMENT))]);

        let registry = provider.snapshot();
        assert_eq!(registry.version(), "remote");
        assert!(registry.get_attribute_precedence(Concept::DbStatement).is_some());
    }

    // Ported from upstream `TestOnSemanticCoreUpdate_EmptyUpdatesRevertToEmbedded`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler_test.go#L804
    #[test]
    fn provider_reverts_to_the_embedded_registry_when_the_configuration_is_removed() {
        let (publisher, provider) = provider();
        publisher.assign::<SemanticCoreDecoder>([("a", payload("remote", DB_STATEMENT))]);
        assert!(!Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));

        publisher.assign::<SemanticCoreDecoder>(std::iter::empty::<(&str, &str)>());
        assert!(Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));
    }

    // Ported from upstream `TestOnSemanticCoreUpdate_AllErrors`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler_test.go#L661
    #[test]
    fn provider_keeps_the_accepted_registry_when_no_configuration_is_valid() {
        let (publisher, provider) = provider();
        publisher.assign::<SemanticCoreDecoder>([("a", payload("kept", DB_STATEMENT))]);
        let kept = provider.snapshot();

        publisher.assign::<SemanticCoreDecoder>([("a", "not json"), ("b", EMPTY_CONCEPTS)]);
        assert!(Arc::ptr_eq(&provider.snapshot(), &kept));
    }

    #[test]
    fn provider_keeps_the_embedded_registry_when_no_configuration_was_ever_valid() {
        let (publisher, provider) = provider();
        publisher.assign::<SemanticCoreDecoder>([("a", "not json")]);
        assert!(Arc::ptr_eq(&provider.snapshot(), &EMBEDDED_REGISTRY));
    }

    // Ported from upstream `TestOnSemanticCoreUpdate_SameHashChangedMappingsApplied`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler_test.go#L679
    #[test]
    fn provider_returns_changed_mappings_under_the_same_declared_hash() {
        let (publisher, provider) = provider();
        publisher.assign::<SemanticCoreDecoder>([("a", payload("same", DB_STATEMENT))]);
        let before = provider.snapshot();

        publisher.assign::<SemanticCoreDecoder>([("a", payload("same", HTTP_METHOD))]);
        let after = provider.snapshot();
        assert_eq!(after.content_hash(), before.content_hash());
        assert!(after.get_attribute_precedence(Concept::HttpMethod).is_some());
    }

    #[test]
    fn selection_logs_its_source_once() {
        let remote = payload("remote", DB_STATEMENT);
        let events = events_while(|| drop(decode_all(&[("a", &remote)])));
        let selections: Vec<_> = events
            .iter()
            .filter(|(level, _)| *level == Level::INFO)
            .map(|(_, fields)| fields.as_str())
            .collect();
        assert_eq!(
            selections,
            [
                r#"message=Semantic registry selected. content_hash="hash-remote" version="remote" source=remote-config (a)"#
            ]
        );
    }

    #[test]
    fn rejection_logs_no_selection() {
        let events = events_while(|| drop(decode_all(&[("a", "not json")])));
        assert!(events.iter().all(|(level, _)| *level != Level::INFO), "{events:?}");
    }
}
