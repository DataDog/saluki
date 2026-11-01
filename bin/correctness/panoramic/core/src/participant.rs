//! Participant type declarations and realization-specific early facts.

use crate::layout::{Os, Realization, RealizationKind};

/// A participant fact available before execution for one realization.
pub struct FactDeclaration {
    /// Name used in participant fact references.
    pub name: &'static str,
    /// Literal value for the realization.
    pub value: &'static str,
}

/// A participant type a case may name, and what it accepts and provides.
pub trait ParticipantType: Send + Sync {
    /// Registry name, as written in a participant's `type`.
    fn name(&self) -> &'static str;

    /// Supported (participant OS, realization kind) pairs. Defaults to none.
    /// Every selected pair must be declared here.
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[]
    }

    /// Names a case may supply under `config.files`. Defaults to none.
    fn config_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// Served endpoints: connection targets and keys of `config.endpoints`. Defaults to none.
    fn serves(&self) -> &'static [&'static str] {
        &[]
    }

    /// Consumed endpoints: keys of `connect`. Defaults to none.
    fn consumes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Names a case may set under `config.params`. Defaults to none.
    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    /// Early facts steps may reference for this realization, sorted by name. Defaults to none.
    /// Called only with realizations from [`Self::supports`].
    fn facts(&self, _realization: Realization) -> &'static [FactDeclaration] {
        &[]
    }

    /// Named processes steps may address with `process`, in sorted order.
    fn processes(&self) -> &'static [&'static str];
}

/// Finds a registered participant type by its case-file name.
pub(crate) fn find_type<'a>(types: &[&'a dyn ParticipantType], name: &str) -> Option<&'a dyn ParticipantType> {
    types
        .iter()
        .copied()
        .find(|participant_type| participant_type.name() == name)
}

/// Describes an unknown participant type and lists the registered alternatives.
pub(crate) fn unknown_type_message(participant: &str, type_name: &str, types: &[&dyn ParticipantType]) -> String {
    format!(
        "participant '{participant}' has unknown type '{type_name}'; registered types: {}",
        types
            .iter()
            .map(|participant_type| participant_type.name())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
