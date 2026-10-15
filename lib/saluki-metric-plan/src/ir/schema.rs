use schemars::{gen::SchemaSettings, schema::RootSchema};

use super::{Plan, PolicyArtifact};
use crate::predicate::Predicate;

/// Returns the JSON Schema of a [`Plan`].
pub fn plan_schema() -> RootSchema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Plan>()
}

/// Returns the JSON Schema of a [`PolicyArtifact`].
pub fn policy_artifact_schema() -> RootSchema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<PolicyArtifact>()
}

/// Returns the JSON Schema of a [`Predicate`].
///
/// Predicates appear in plans as operator parameters, which the plan schema leaves open, because each operator's
/// parameter schema defines them. This schema lets tools check predicates on their own.
pub fn predicate_schema() -> RootSchema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Predicate>()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Set this environment variable to rewrite the checked-in schemas instead of comparing against them.
    const UPDATE_VARIABLE: &str = "SALUKI_METRIC_PLAN_UPDATE_SCHEMA";

    #[test]
    fn checked_in_schemas_are_current() {
        let schemas = [
            ("plan.schema.json", plan_schema()),
            ("policy-artifact.schema.json", policy_artifact_schema()),
            ("predicate.schema.json", predicate_schema()),
        ];

        for (file, schema) in schemas {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema").join(file);
            let generated = serde_json::to_string_pretty(&schema).expect("schemas serialize") + "\n";

            if std::env::var_os(UPDATE_VARIABLE).is_some() {
                std::fs::write(&path, &generated).expect("schema file is writable");
                continue;
            }

            let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                checked_in == generated,
                "The checked-in JSON Schema at {} does not match the IR types. If you changed the IR on purpose, \
                 regenerate the schema with:\n\n    {}=1 cargo test -p saluki-metric-plan \
                 checked_in_schemas_are_current\n",
                path.display(),
                UPDATE_VARIABLE
            );
        }
    }

    #[test]
    fn schemas_reject_unknown_members() {
        let schema = serde_json::to_value(plan_schema()).unwrap();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            schema["definitions"]["Node"]["additionalProperties"],
            serde_json::json!(false)
        );

        let schema = serde_json::to_value(policy_artifact_schema()).unwrap();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            schema["definitions"]["ArtifactNode"]["additionalProperties"],
            serde_json::json!(false)
        );
    }
}
