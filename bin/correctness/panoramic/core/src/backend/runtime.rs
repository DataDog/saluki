//! Prepares each realization kind's scope and runs its participants.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use async_trait::async_trait;
use saluki_error::GenericError;
use serde_json::Value;

use super::{LateFact, COURIER_LINUX_BINARY, FIXUP_IMAGE};
use crate::{
    endpoint::RunnerHostPort,
    handle::Primitives,
    layout::{Os, Realization, RealizationKind},
    ledger::ParticipantRemoval,
    lifecycle::{CaseControl, NativeInputs},
    observe::ParticipantObservation,
    participant::ProcessSpec,
    plan::ScopeLayout,
    recipe::Recipe,
    tasks::CaseTasks,
};

/// One realization kind's part of a case's isolation scope, prepared once per case.
pub(crate) trait ScopeRuntime: Send + Sync {
    /// Builds a participant whose resources are allocated by `create`.
    fn participant(&self, request: ParticipantRequest<'_>) -> Result<Arc<dyn RuntimeParticipant>, String>;
}

pub(crate) struct ParticipantRequest<'a> {
    pub(crate) name: &'a str,
    pub(crate) realization: Realization,
    pub(crate) recipe: Recipe,
    pub(crate) runner_host_ports: Vec<RunnerHostPort>,
    pub(crate) inputs: &'a NativeInputs,
    pub(crate) options: &'a BTreeMap<String, Value>,
}

#[async_trait]
pub(crate) trait RuntimeParticipant: Primitives {
    /// How reclaim stops and removes this participant; available before `create`.
    fn removal(&self) -> ParticipantRemoval;
    /// Creates the participant and returns the late facts known after creation.
    async fn create(&self) -> Result<Vec<(LateFact, String)>, GenericError>;
    /// Starts the participant and returns the late facts known after start.
    async fn start(&self) -> Result<Vec<(LateFact, String)>, GenericError>;
    /// Follows the started participant's exits and output under the case's tasks.
    async fn observe(
        self: Arc<Self>, specs: &'static [ProcessSpec], tasks: &CaseTasks, output_dir: &Path,
    ) -> ParticipantObservation;
}

/// Operating systems supported by an implemented runtime.
pub(crate) fn implemented(kind: RealizationKind) -> Option<&'static [Os]> {
    match kind {
        RealizationKind::LinuxContainer => Some(&[Os::Linux]),
        RealizationKind::HostProcessGroup | RealizationKind::WindowsContainer | RealizationKind::KubernetesPod => None,
    }
}

pub(crate) async fn prepare(
    kind: RealizationKind, scope: &ScopeLayout, participants: &[&str], inputs: &NativeInputs, control: &CaseControl,
) -> Result<Box<dyn ScopeRuntime>, String> {
    match kind {
        RealizationKind::LinuxContainer => {
            let image = inputs.image(FIXUP_IMAGE)?;
            let courier = inputs.container_binary(COURIER_LINUX_BINARY)?;
            let id = scope.id();
            control.ledger.linux_scope(inputs.backends.clone(), scope.clone());
            let scope = control
                .ledger
                .create(
                    &id,
                    &control.cancel,
                    inputs
                        .backends
                        .prepare_linux_scope(scope, participants, image, courier.clone()),
                )
                .await
                .ok_or_else(|| "cancelled while preparing the isolation scope".to_string())?
                .map_err(|error| format!("failed to prepare isolation scope '{id}': {error:#}"))?;
            Ok(Box::new(scope))
        }
        RealizationKind::HostProcessGroup | RealizationKind::WindowsContainer | RealizationKind::KubernetesPod => {
            Err(format!("panoramic cannot prepare {} scopes natively", kind.name()))
        }
    }
}
