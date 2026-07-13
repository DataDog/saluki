//! Subsystem-scoped diagnostics control surface.

// TODO: probably rework process name construction to use `SubsystemIdentifier` under the hood,
// and expose that, in addition to process ID, as a task-local we can access so that we can
// make `DiagnosticEmitter::new` require no parameters at all while still doing the right thing

use snafu::{OptionExt as _, Snafu};
use stringtheory::MetaString;

use super::{DiagnosticCollector, DiagnosticEvent};
use crate::{
    runtime::state::{DataspaceRegistry, Identifier, IdentifierFilter, Subscription},
    support::SubsystemIdentifier,
};

/// Errors that can occur when creating a [`DiagnosticsEmitter`].
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum DiagnosticsEmitterError {
    /// No dataspace was available in the current context.
    ///
    /// An emitter can only be created within a running supervision tree, where an ambient dataspace is established.
    /// This indicates that creation was attempted outside of one.
    #[snafu(display("no dataspace available in the current context (not running inside a supervision tree)"))]
    NoDataspace,
}

/// A subsystem-scoped control surface for exposing diagnostics.
///
/// A `DiagnosticsEmitter` is created for a single subsystem, identified by a [`SubsystemIdentifier`], and is attached
/// to a specific dataspace. It hides the boilerplate of interacting with the dataspace directly, while still using it
/// under the hood so that other subsystems can subscribe to what is exposed in a decoupled, eventually consistent way.
///
/// It exposes two capabilities:
///
/// - **Collectors**: named, on-demand producers of artifact bytes (see [`register_collector`]), stored as persistent
///   dataspace assertions and automatically withdrawn when the owning process exits.
/// - **Events**: abstract, point-in-time [`DiagnosticEvent`]s (see [`emit`]), delivered as transient dataspace
///   messages to any subscribers present at the time of emission.
///
/// [`register_collector`]: Self::register_collector
/// [`emit`]: Self::emit
///
/// # Example
///
/// ```
/// use saluki_core::diagnostic::{DiagnosticDetails, DiagnosticEvent, DiagnosticsEmitter};
/// use saluki_core::runtime::state::DataspaceRegistry;
/// use saluki_core::support::SubsystemIdentifier;
///
/// let dataspace = DataspaceRegistry::new();
/// let emitter = DiagnosticsEmitter::from_dataspace(
///     SubsystemIdentifier::from_segments(["my-subsystem"]),
///     dataspace,
/// );
///
/// // Expose an artifact that is produced on demand:
/// emitter.register_collector("state.json", || b"{}");
///
/// // Emit a point-in-time event:
/// emitter.emit(DiagnosticEvent::new("credentials rejected", DiagnosticDetails::InvalidApiKey));
/// ```
#[derive(Clone)]
pub struct DiagnosticsEmitter {
    base_id: MetaString,
    dataspace: DataspaceRegistry,
}

impl DiagnosticsEmitter {
    /// Creates an emitter for the given subsystem, attaching to the current dataspace.
    ///
    /// # Errors
    ///
    /// If no dataspace is available, an error is returned.
    pub fn from_current(id: SubsystemIdentifier) -> Result<Self, DiagnosticsEmitterError> {
        let dataspace = DataspaceRegistry::try_current().context(NoDataspace)?;
        Ok(Self::from_dataspace(id, dataspace))
    }

    /// Creates an emitter for the given subsystem from an already-held dataspace handle.
    pub fn from_dataspace(id: SubsystemIdentifier, dataspace: DataspaceRegistry) -> Self {
        let base_id = id.to_string();
        Self {
            base_id: base_id.into(),
            dataspace,
        }
    }

    /// Registers a collector for a given artifact.
    ///
    /// The collector is exposed until it is explicitly removed via [`unregister_collector`], or until the owning
    /// process exits, whichever comes first. Registering a collector with an artifact name that is already registered
    /// by this subsystem replaces the previous one.
    ///
    /// Care should be taken when registering a collector:
    ///
    /// - the given artifact name _should_ be unique within the overall system, and should be generally suitable as a
    ///   file name when possible (artifact names are sanitized/normalized where necessary, but may lose useful
    ///   information in the process)
    /// - the collection function (`collect_fn`) will be run synchronously and should return promptly, as it can delay
    ///   the collection of artifacts for the whole system
    ///
    /// [`unregister_collector`]: Self::unregister_collector
    pub fn register_collector<F, T>(&self, artifact_name: impl Into<String>, collect_fn: F)
    where
        F: Fn() -> T + Send + Sync + 'static,
        T: Into<Vec<u8>>,
    {
        let collector = DiagnosticCollector::new(artifact_name, collect_fn);
        let id = self.build_collector_identifier(collector.artifact_name());
        self.dataspace.assert(collector, id);
    }

    /// Removes a previously registered collector by name
    ///
    /// Does nothing if no collector with that name is currently registered by this subsystem.
    pub fn unregister_collector(&self, artifact_name: impl AsRef<str>) {
        let id = self.build_collector_identifier(artifact_name.as_ref());
        self.dataspace.retract::<DiagnosticCollector>(id);
    }

    /// Emits a diagnostic event.
    ///
    /// Diagnostics events are transient and only delivered to active listeners.
    pub fn emit(&self, event: DiagnosticEvent) {
        self.dataspace.send(event, self.base_id.clone());
    }

    fn build_collector_identifier(&self, artifact_name: &str) -> Identifier {
        Identifier::named(format!("{}-{}", self.base_id, artifact_name))
    }
}

/// Subscribes to diagnostic events matching the given filter, using the current dataspace.
///
/// This is the counterpart to [`DiagnosticsEmitter::emit`] for consumers that want to observe events without holding a
/// [`DataspaceRegistry`] directly.
///
/// # Errors
///
/// If no dataspace is available, an error is returned.
pub fn subscribe_events(filter: IdentifierFilter) -> Result<Subscription<DiagnosticEvent>, DiagnosticsEmitterError> {
    let dataspace = DataspaceRegistry::try_current().context(NoDataspace)?;
    Ok(dataspace.subscribe::<DiagnosticEvent>(filter))
}

#[cfg(test)]
mod tests {
    use tokio_test::{assert_pending, assert_ready, assert_ready_eq, task::spawn as test_spawn};

    use super::*;
    use crate::{
        diagnostic::DiagnosticDetails,
        runtime::{
            state::{DataspaceUpdate, CURRENT_DATASPACE},
            ProcessId,
        },
    };

    fn emitter(dataspace: DataspaceRegistry) -> DiagnosticsEmitter {
        DiagnosticsEmitter::from_dataspace(SubsystemIdentifier::from_segments(["sub"]), dataspace)
    }

    #[test]
    fn from_current_without_dataspace_errors() {
        let result = DiagnosticsEmitter::from_current(SubsystemIdentifier::from_segments(["sub"]));
        assert!(matches!(result, Err(DiagnosticsEmitterError::NoDataspace)));
    }

    #[test]
    fn from_current_inside_dataspace_succeeds() {
        let registry = DataspaceRegistry::new();
        CURRENT_DATASPACE.sync_scope(registry, || {
            assert!(DiagnosticsEmitter::from_current(SubsystemIdentifier::from_segments(["sub"])).is_ok());
        });
    }

    #[test]
    fn register_collector_is_discoverable() {
        let registry = DataspaceRegistry::new();
        emitter(registry.clone()).register_collector("state.json", || b"hello");

        let collectors = registry.current_values::<DiagnosticCollector>(IdentifierFilter::all());
        assert_eq!(collectors.len(), 1);
        assert_eq!(collectors[0].artifact_name(), "state.json");
        assert_eq!(collectors[0].collect(), b"hello".to_vec());
    }

    #[test]
    fn multiple_collectors_coexist() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());
        emitter.register_collector("tags.json", || vec![1]);
        emitter.register_collector("eds.json", || vec![2]);

        let mut names: Vec<String> = registry
            .current_values::<DiagnosticCollector>(IdentifierFilter::all())
            .iter()
            .map(|c| c.artifact_name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["eds.json".to_string(), "tags.json".to_string()]);
    }

    #[test]
    fn reregister_same_artifact_updates() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());
        emitter.register_collector("state.json", || b"v1");
        emitter.register_collector("state.json", || b"v2");

        let collectors = registry.current_values::<DiagnosticCollector>(IdentifierFilter::all());
        assert_eq!(collectors.len(), 1);
        assert_eq!(collectors[0].collect(), b"v2".to_vec());
    }

    #[test]
    fn unregister_collector_retracts() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());

        let mut sub = registry.subscribe::<DiagnosticCollector>(IdentifierFilter::all());
        emitter.register_collector("state.json", || vec![0]);
        emitter.unregister_collector("state.json");

        // First update: the assertion, under the derived `<subsystem>-<artifact>` identifier.
        let mut recv = test_spawn(sub.recv());
        match assert_ready!(recv.poll()) {
            Some(DataspaceUpdate::Asserted(id, collector)) => {
                assert_eq!(id, Identifier::named("sub-state.json"));
                assert_eq!(collector.artifact_name(), "state.json");
            }
            _ => panic!("expected an assertion first"),
        }
        drop(recv);

        // Second update: the retraction.
        let mut recv = test_spawn(sub.recv());
        match assert_ready!(recv.poll()) {
            Some(DataspaceUpdate::Retracted(id)) => assert_eq!(id, Identifier::named("sub-state.json")),
            _ => panic!("expected a retraction second"),
        }
    }

    #[test]
    fn register_collector_is_tagged_to_current_process() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());

        // The collector is asserted under the current process, so it is withdrawn when that process exits.
        let pid = ProcessId::current();
        emitter.register_collector("state.json", || vec![0]);
        assert_eq!(
            registry
                .current_values::<DiagnosticCollector>(IdentifierFilter::all())
                .len(),
            1
        );

        // Simulating that process exiting withdraws the collector automatically.
        registry.retract_all_for_process(pid);
        assert!(registry
            .current_values::<DiagnosticCollector>(IdentifierFilter::all())
            .is_empty());
    }

    #[test]
    fn emit_delivers_event_to_subscriber() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());

        let mut sub = registry.subscribe::<DiagnosticEvent>(IdentifierFilter::all());
        emitter.emit(DiagnosticEvent::new(
            "credentials rejected",
            DiagnosticDetails::InvalidApiKey,
        ));

        let mut recv = test_spawn(sub.recv());
        assert_ready_eq!(
            recv.poll(),
            Some(DataspaceUpdate::Message(
                Identifier::named("sub"),
                DiagnosticEvent::new("credentials rejected", DiagnosticDetails::InvalidApiKey)
            ))
        );
    }

    #[test]
    fn subscribe_events_receives_emitted_event() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());

        CURRENT_DATASPACE.sync_scope(registry, || {
            let mut sub = subscribe_events(IdentifierFilter::all()).expect("dataspace should be available");
            emitter.emit(DiagnosticEvent::new("boom", DiagnosticDetails::InvalidApiKey));

            let mut recv = test_spawn(sub.recv());
            assert_ready_eq!(
                recv.poll(),
                Some(DataspaceUpdate::Message(
                    Identifier::named("sub"),
                    DiagnosticEvent::new("boom", DiagnosticDetails::InvalidApiKey)
                ))
            );
        });
    }

    #[test]
    fn emit_is_transient() {
        let registry = DataspaceRegistry::new();
        let emitter = emitter(registry.clone());

        // Emit before anyone is subscribed.
        emitter.emit(DiagnosticEvent::new("boom", DiagnosticDetails::InvalidApiKey));

        // Events are never stored.
        assert!(registry
            .current_values::<DiagnosticEvent>(IdentifierFilter::all())
            .is_empty());

        // A subscriber that appears afterwards does not receive the already-sent event.
        let mut sub = registry.subscribe::<DiagnosticEvent>(IdentifierFilter::all());
        let mut recv = test_spawn(sub.recv());
        assert_pending!(recv.poll());
    }
}
