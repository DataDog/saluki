use std::ops::RangeInclusive;

use serde_json::{Map, Value};
use snafu::Snafu;

use super::{
    id::{ArtifactNodeId, ArtifactPortRef},
    plan::BlobError,
};
use crate::canonical::CanonicalError;

/// The IR version this crate reads and writes.
///
/// The version increases only for structural changes to the IR. New operators, parameters, and predicate leaves are
/// additive: they keep the version, and the operator registry checks them.
pub const IR_VERSION: u32 = 1;

/// The IR versions this crate accepts. Documents of an older accepted version are migrated to [`IR_VERSION`].
pub const ACCEPTED_IR_VERSIONS: RangeInclusive<u32> = 1..=IR_VERSION;

/// One step of migration, from IR version `from` to version `from + 1`.
#[derive(Clone, Copy, Debug)]
pub struct MigrationStep {
    /// The version this step migrates from.
    pub from: u32,

    /// Rewrites a document of version `from` into version `from + 1`, except for `ir_version`, which the caller sets.
    ///
    /// Returns a description of the problem if the document cannot be migrated.
    pub apply: fn(&mut Map<String, Value>) -> Result<(), String>,
}

/// The migration steps, one for each accepted version older than [`IR_VERSION`].
///
/// Version 1 is the first version, so there are no steps yet and migrating a version 1 document is the identity.
pub const MIGRATIONS: &[MigrationStep] = &[];

/// An error reading an IR document.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)), visibility(pub(crate)))]
pub enum IrError {
    /// The document is not valid JSON.
    #[snafu(display("Failed to parse the document as JSON: {}", source))]
    Parse {
        /// The underlying error.
        source: serde_json::Error,
    },

    /// The document is not a JSON object.
    #[snafu(display("The document must be a JSON object."))]
    NotAnObject,

    /// The document has no IR version.
    #[snafu(display("The document has no `ir_version`. Add `\"ir_version\": {}`.", IR_VERSION))]
    MissingVersion,

    /// The document's IR version is not accepted.
    #[snafu(display(
        "IR version {} is not supported: this version of the plan crate accepts IR versions {} through {}.",
        version,
        ACCEPTED_IR_VERSIONS.start(),
        ACCEPTED_IR_VERSIONS.end()
    ))]
    UnsupportedVersion {
        /// The document's version, as written.
        version: Value,
    },

    /// A migration step failed.
    #[snafu(display("Failed to migrate the document from IR version {}: {}", from, message))]
    Migration {
        /// The version being migrated from.
        from: u32,

        /// What went wrong.
        message: String,
    },

    /// The document does not match the IR.
    #[snafu(display("The document is not a valid {}: {}", what, source))]
    Invalid {
        /// What the document was read as.
        what: &'static str,

        /// The underlying error.
        source: serde_json::Error,
    },

    /// An artifact node's parameters carry the resolved value of a configuration binding.
    #[snafu(display(
        "Artifact node `{}` carries a resolved configuration value. Artifacts refer to configuration by binding only, \
         as {{\"config\": \"<binding>\"}}: remove the `resolved` member, or write the value as a literal.",
        node
    ))]
    ResolvedInArtifact {
        /// The node.
        node: ArtifactNodeId,
    },

    /// An artifact edge leaves one of its scope's exits.
    #[snafu(display(
        "Artifact edge {} leaves `{}`, a scope exit. Exits are where edges leave the scope, so an exit is only ever an \
         edge's target: write the edge from the port that feeds the exit.",
        index,
        exit
    ))]
    EdgeFromExit {
        /// The edge's position in the artifact's edge list, counting from zero.
        index: usize,

        /// The exit, as written.
        exit: ArtifactPortRef,
    },

    /// An artifact's blob table does not match its content or its references.
    #[snafu(display("The artifact's blob table is invalid: {}", source))]
    BlobTable {
        /// The underlying error.
        source: BlobError,
    },

    /// The document has no canonical form, so it has no hash.
    #[snafu(display("The document cannot be hashed: {}", source))]
    Unhashable {
        /// The underlying error.
        source: CanonicalError,
    },
}

/// Migrates an IR document to [`IR_VERSION`], in place, and returns the version it was written in.
///
/// # Errors
///
/// Returns an error if the document is not an object, has no `ir_version`, has a version outside
/// [`ACCEPTED_IR_VERSIONS`], or cannot be migrated.
pub fn migrate(document: &mut Value) -> Result<u32, IrError> {
    let members = document.as_object_mut().ok_or(IrError::NotAnObject)?;
    let written = members.get("ir_version").ok_or(IrError::MissingVersion)?;
    let version = written
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| ACCEPTED_IR_VERSIONS.contains(version))
        .ok_or_else(|| IrError::UnsupportedVersion {
            version: written.clone(),
        })?;

    let mut current = version;
    while current < IR_VERSION {
        let step = MIGRATIONS
            .iter()
            .find(|step| step.from == current)
            .ok_or_else(|| IrError::Migration {
                from: current,
                message: "no migration step is defined for this version".to_string(),
            })?;
        (step.apply)(members).map_err(|message| IrError::Migration { from: current, message })?;
        current += 1;
        members.insert("ir_version".to_string(), Value::from(current));
    }

    Ok(version)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn current_documents_migrate_unchanged() {
        let original = json!({"ir_version": 1, "nodes": [], "edges": []});
        let mut document = original.clone();
        assert_eq!(migrate(&mut document).unwrap(), 1);
        assert_eq!(document, original);
    }

    #[test]
    fn documents_outside_the_accepted_range_are_rejected() {
        for version in [json!(0), json!(2), json!(-1), json!("1"), json!(1.5), json!(null)] {
            let mut document = json!({"ir_version": version.clone()});
            let error = migrate(&mut document).unwrap_err();
            assert!(
                matches!(error, IrError::UnsupportedVersion { .. }),
                "{}: {}",
                version,
                error
            );
            assert!(
                error.to_string().contains("accepts IR versions 1 through 1"),
                "{}",
                error
            );
        }

        assert!(matches!(migrate(&mut json!({})).unwrap_err(), IrError::MissingVersion));
        assert!(matches!(migrate(&mut json!([])).unwrap_err(), IrError::NotAnObject));
    }

    #[test]
    fn every_older_accepted_version_has_a_migration_step() {
        for version in ACCEPTED_IR_VERSIONS.filter(|version| *version < IR_VERSION) {
            assert!(
                MIGRATIONS.iter().any(|step| step.from == version),
                "no migration step from version {}",
                version
            );
        }
        for step in MIGRATIONS {
            assert!(step.from < IR_VERSION && ACCEPTED_IR_VERSIONS.contains(&step.from));
        }
        assert!(ACCEPTED_IR_VERSIONS.contains(&IR_VERSION));
    }
}
