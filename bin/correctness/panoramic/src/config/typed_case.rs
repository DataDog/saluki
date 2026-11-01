//! Typed case schemas and the runners built from loaded cases.

use std::{collections::BTreeSet, path::Path, time::Duration};

use saluki_error::{generic_error, GenericError};

use super::{
    case::{case_name, deserialize_case, CaseConfig, CaseSchema, Os, TypedCase, TypedCaseLoader},
    integration_bridge, os_for_runtime, parse_case, IntegrationConfig, MatrixVariants, LINUX_RUNTIME, MAC_RUNTIME,
    WINDOWS_RUNTIME,
};
use crate::{
    correctness::{case::CorrectnessTestCase, config::Config as CorrectnessConfig},
    image_override::ImageOverrides,
    integration::{IntegrationSettings, IntegrationTestCase},
    test::{Test, TestSuite},
};

/// Time budget for correctness cases, which run two isolation groups of several containers each.
pub(crate) const CORRECTNESS_TIMEOUT: Duration = Duration::from_mins(20);

/// Parsed integration or correctness case data.
pub(crate) enum TypedConfig {
    Integration(IntegrationConfig),
    Correctness(CorrectnessConfig),
}

/// A case loaded with this binary's typed schemas.
pub(crate) type LoadedCase = CaseConfig<TypedConfig>;

/// Loads integration, correctness and correctness-matrix files.
pub(crate) struct TypedConfigLoader;

impl TypedCaseLoader for TypedConfigLoader {
    type Config = TypedConfig;

    fn load(
        &self, case_type: &serde_json::Value, content: &str, directory: &str, source: &Path,
    ) -> Result<Vec<TypedCase<Self::Config>>, GenericError> {
        let serde_json::Value::String(test_type) = case_type else {
            return Err(generic_error!(
                "'type' in {} must be a string: 'integration', 'correctness' or 'correctness_matrix'; \
                 a case that declares participants has no top-level 'type'",
                source.display()
            ));
        };
        match test_type.as_str() {
            "integration" => {
                let config: IntegrationConfig = parse_case(content, source)?;
                let name = case_name(Some(&config.name), directory, source)?;
                if config.runtimes.is_empty() {
                    return Err(generic_error!(
                        "integration test '{}' in {} has empty runtimes list",
                        config.name,
                        source.display()
                    ));
                }
                let subject_os = config
                    .runtimes
                    .iter()
                    .map(|runtime| {
                        os_for_runtime(runtime).ok_or_else(|| {
                            generic_error!(
                                "integration test '{}' in {} declares unknown runtime '{}' (expected '{}', '{}', or '{}')",
                                config.name,
                                source.display(),
                                runtime,
                                LINUX_RUNTIME,
                                MAC_RUNTIME,
                                WINDOWS_RUNTIME
                            )
                        })
                    })
                    .collect::<Result<BTreeSet<_>, _>>()?;
                Ok(vec![TypedCase {
                    name,
                    description: config.description.clone(),
                    suite: TestSuite::Integration.name().to_string(),
                    timeout: config.timeout.0,
                    subject_os,
                    disabled: false,
                    config: TypedConfig::Integration(config),
                }])
            }
            "correctness" => {
                let config = parse_case(content, source)?;
                let name = case_name(None, directory, source)?;
                Ok(vec![from_correctness(name, config)])
            }
            "correctness_matrix" => {
                // Parse each schema from YAML text to preserve scalar coercion.
                let base: CorrectnessConfig = parse_case(content, source)?;
                let matrix: MatrixVariants = deserialize_case(content, source)?;
                let base_name = case_name(None, directory, source)?;
                if matrix.variants.is_empty() {
                    return Err(generic_error!(
                        "correctness_matrix '{}' in {} has no variants defined",
                        base_name,
                        source.display()
                    ));
                }
                Ok(matrix
                    .expand(&base, &base_name)
                    .into_iter()
                    .map(|(name, config)| from_correctness(name, config))
                    .collect())
            }
            other => Err(generic_error!(
                "Unknown test type '{}' in {} (expected 'integration', 'correctness', or 'correctness_matrix'; \
                 a case that declares participants has no top-level 'type')",
                other,
                source.display()
            )),
        }
    }
}

fn from_correctness(name: String, config: CorrectnessConfig) -> TypedCase<TypedConfig> {
    TypedCase {
        name,
        description: None,
        suite: TestSuite::Correctness.name().to_string(),
        timeout: CORRECTNESS_TIMEOUT,
        subject_os: BTreeSet::from([Os::Linux]),
        disabled: config.disabled,
        config: TypedConfig::Correctness(config),
    }
}

/// Runtime eligibility, participant translation and runner construction for loaded cases.
pub(crate) trait RunnableCase {
    /// Scopes integration and participant cases to their declared OSes;
    /// enabled correctness cases accept every runtime.
    fn is_eligible_for(&self, runtime: &str) -> bool;

    /// Translates a participant case into the configuration its integration runner receives.
    ///
    /// # Errors
    ///
    /// Returns an error naming the case and source if it uses another schema or suite, has unsupported
    /// participant settings or steps, or omits the requested subject OS.
    /// See [`integration_bridge::integration_config`].
    fn integration_config(&self, os: Os) -> Result<IntegrationConfig, GenericError>;

    /// Builds a runner from a copy of the case data, applying image choices to that copy.
    ///
    /// # Errors
    ///
    /// Participant cases require a recognized subject OS runtime and propagate [`Self::integration_config`] errors.
    fn build_test(
        &self, integration_runtime: &str, settings: &IntegrationSettings, overrides: &ImageOverrides<'_>,
    ) -> Result<Box<dyn Test>, GenericError>;
}

impl RunnableCase for LoadedCase {
    fn is_eligible_for(&self, runtime: &str) -> bool {
        if self.is_disabled() {
            return false;
        }
        match self.schema() {
            CaseSchema::Typed(TypedConfig::Integration(_)) | CaseSchema::Participants { .. } => {
                os_for_runtime(runtime).is_some_and(|os| self.subject_os().contains(&os))
            }
            CaseSchema::Typed(TypedConfig::Correctness(_)) => true,
        }
    }

    fn integration_config(&self, os: Os) -> Result<IntegrationConfig, GenericError> {
        let CaseSchema::Participants { file, procedure } = self.schema() else {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: integration translation requires a case that declares participants",
                self.name(),
                self.source().display()
            ));
        };
        if self.suite() != TestSuite::Integration.name() {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: the integration runner runs only integration-suite cases",
                self.name(),
                self.source().display()
            ));
        }
        if !self.subject_os().contains(&os) {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: it does not declare subject OS '{}'",
                self.name(),
                self.source().display(),
                os.name()
            ));
        }
        integration_bridge::integration_config(file, self.name(), self.source(), procedure, os).map_err(|reason| {
            generic_error!(
                "Cannot run case '{}' from {}: {}",
                self.name(),
                self.source().display(),
                reason
            )
        })
    }

    fn build_test(
        &self, integration_runtime: &str, settings: &IntegrationSettings, overrides: &ImageOverrides<'_>,
    ) -> Result<Box<dyn Test>, GenericError> {
        match self.schema() {
            CaseSchema::Typed(TypedConfig::Integration(config)) => {
                Ok(Box::new(IntegrationTestCase::new(config.clone(), settings)))
            }
            CaseSchema::Typed(TypedConfig::Correctness(config)) => {
                let mut config = config.clone();
                overrides.apply_correctness(&mut config);
                Ok(Box::new(CorrectnessTestCase::new(self.name().to_string(), config)))
            }
            CaseSchema::Participants { .. } => {
                let os = os_for_runtime(integration_runtime).ok_or_else(|| {
                    generic_error!(
                        "Cannot run case '{}' from {}: runtime '{}' is not a subject OS",
                        self.name(),
                        self.source().display(),
                        integration_runtime
                    )
                })?;
                Ok(Box::new(IntegrationTestCase::new(
                    self.integration_config(os)?,
                    settings,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        config::{
            build_runners,
            case::{CaseLoader, Registries},
            default_host_runtime,
            tests::{write_case, PARTICIPANT_FIELDS},
        },
        participants, steps,
    };

    const CORRECTNESS_FIELDS: &str =
        "runtime: docker\nanalysis_mode: metrics\nbaseline: {image: 'base:file'}\ncomparison: {image: 'comp:file'}\n";

    #[test]
    fn integration_case_records_its_declared_name_description_timeout_and_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            "type: integration\nname: alpha\ndescription: A portable case\ntimeout: 7s\nruntimes: [linux, mac]\nprocedure: []\n",
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "alpha");
        assert_eq!(case.description(), Some("A portable case"));
        assert_eq!(case.timeout(), Duration::from_secs(7));
        assert_eq!(case.suite(), TestSuite::Integration.name());
        assert_eq!(*case.subject_os(), BTreeSet::from([Os::Linux, Os::MacOs]));
        assert!(!case.is_disabled());
        let canonical = path.canonicalize().unwrap();
        assert_eq!(case.source(), canonical);
        assert_eq!(case.source().parent().unwrap(), canonical.parent().unwrap());
    }

    #[test]
    fn integration_translation_requires_a_participant_case() {
        for yaml in [
            "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n".to_string(),
            format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        ] {
            let cases = crate::CASE_LOADER
                .load_content(&yaml, "alpha", Path::new("alpha/config.yaml"))
                .unwrap();
            assert_eq!(cases.len(), 1);
            assert_eq!(
                cases[0].integration_config(Os::Linux).unwrap_err().to_string(),
                "Cannot run case 'alpha' from alpha/config.yaml: \
                 integration translation requires a case that declares participants"
            );
        }
    }

    #[test]
    fn a_participant_case_in_another_suite_cannot_build_an_integration_runner() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "alpha", PARTICIPANT_FIELDS);
        let loader = CaseLoader::new(
            Registries {
                participant_types: participants::REGISTERED_TYPES,
                steps: steps::REGISTERED_STEPS,
            },
            TypedConfigLoader,
            TestSuite::Correctness.name(),
        );
        let cases = loader.load_file(&path).unwrap();
        let error = build_runners(&cases, LINUX_RUNTIME, &[])
            .err()
            .expect("the case and runner suites must agree");
        assert_eq!(
            error.to_string(),
            format!(
                "Failed to build test runners:\n  - Cannot run case 'alpha' from {}: \
                 the integration runner runs only integration-suite cases",
                path.canonicalize().unwrap().display()
            )
        );
    }

    #[test]
    fn participant_translation_requires_a_declared_subject_os() {
        let cases = crate::CASE_LOADER
            .load_content(PARTICIPANT_FIELDS, "alpha", Path::new("alpha/config.yaml"))
            .unwrap();
        assert_eq!(
            cases[0].integration_config(Os::Windows).unwrap_err().to_string(),
            "Cannot run case 'alpha' from alpha/config.yaml: it does not declare subject OS 'windows'"
        );
        let message = build_runners(&cases, WINDOWS_RUNTIME, &[])
            .err()
            .expect("runner construction must translate for the requested OS")
            .to_string();
        assert_eq!(message, "Failed to build test runners:\n  - Cannot run case 'alpha' from alpha/config.yaml: it does not declare subject OS 'windows'");
    }

    #[test]
    fn participant_runners_require_a_subject_os_runtime() {
        let cases = crate::CASE_LOADER
            .load_content(PARTICIPANT_FIELDS, "alpha", Path::new("alpha/config.yaml"))
            .unwrap();
        for runtime in ["docker", "macos", "unknown"] {
            let message = build_runners(&cases, runtime, &[])
                .err()
                .expect("invalid subject OS runtime")
                .to_string();
            assert_eq!(message, format!("Failed to build test runners:\n  - Cannot run case 'alpha' from alpha/config.yaml: runtime '{runtime}' is not a subject OS"));
        }
    }

    #[test]
    fn participant_cases_are_eligible_only_for_their_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "alpha",
            &PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]"),
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        for (runtime, eligible) in [
            (LINUX_RUNTIME, true),
            (MAC_RUNTIME, false),
            (WINDOWS_RUNTIME, true),
            ("unknown", false),
        ] {
            assert_eq!(cases[0].is_eligible_for(runtime), eligible, "{runtime}");
        }
    }

    #[test]
    fn correctness_case_takes_its_name_from_the_directory_and_the_fixed_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "beta", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.name(), "beta");
        assert_eq!(case.description(), None);
        assert_eq!(case.timeout(), CORRECTNESS_TIMEOUT);
        assert_eq!(case.timeout(), Duration::from_mins(20));
        assert_eq!(case.suite(), TestSuite::Correctness.name());
        assert_eq!(*case.subject_os(), BTreeSet::from([Os::Linux]));
        assert!(!case.is_disabled());
    }

    #[test]
    fn matrix_file_yields_one_case_per_variant_in_declaration_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "matrix",
            &format!(
                "type: correctness_matrix\n{CORRECTNESS_FIELDS}\nvariants:\n  - name: second\n    env: {{VARIANT: second}}\n  - name: first\n    env: {{VARIANT: first}}\n"
            ),
        );
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(
            cases.iter().map(CaseConfig::name).collect::<Vec<_>>(),
            ["matrix/second", "matrix/first"]
        );
        let source = path.canonicalize().unwrap();
        let runners = build_runners(&cases, LINUX_RUNTIME, &[]).unwrap();
        for ((case, runner), variant) in cases.iter().zip(&runners).zip(["second", "first"]) {
            assert_eq!(case.source(), source);
            assert_eq!(case.timeout(), CORRECTNESS_TIMEOUT);
            assert_eq!(case.suite(), TestSuite::Correctness.name());
            assert_eq!(case.description(), None);
            assert_eq!(*case.subject_os(), BTreeSet::from([Os::Linux]));
            assert!(!case.is_disabled());
            assert_eq!(runner.name(), format!("matrix/{variant}"));
            assert_eq!(runner.case_path(), source.parent().unwrap());

            // Check the variant data build_test consumes; CorrectnessTestCase exposes its target environment.
            let CaseSchema::Typed(TypedConfig::Correctness(config)) = case.schema() else {
                panic!("expected correctness data");
            };
            let runner = CorrectnessTestCase::new(case.name().to_string(), config.clone());
            for target in [&runner.config.baseline, &runner.config.comparison] {
                let (target, _) = runner.target_config(target).unwrap();
                assert_eq!(target.additional_env_vars, [format!("VARIANT={variant}")]);
            }
        }
    }

    #[test]
    fn disabled_correctness_cases_load_but_are_never_eligible() {
        let dir = tempfile::tempdir().unwrap();
        for (kind, variants, names) in [
            ("correctness", "", vec!["disabled"]),
            (
                "correctness_matrix",
                "variants: [{name: second}, {name: first}]\n",
                vec!["disabled/second", "disabled/first"],
            ),
        ] {
            let path = write_case(
                dir.path(),
                "disabled",
                &format!("type: {kind}\n{CORRECTNESS_FIELDS}disabled: true\n{variants}"),
            );
            let cases = crate::CASE_LOADER.load_file(&path).unwrap();
            assert_eq!(cases.iter().map(CaseConfig::name).collect::<Vec<_>>(), names);
            for case in cases {
                assert!(case.is_disabled());
                for runtime in [LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME, "unknown"] {
                    assert!(!case.is_eligible_for(runtime), "{kind} under {runtime}");
                }
            }
        }
    }

    #[test]
    fn eligibility_scopes_integration_cases_to_their_runtimes_and_not_correctness_cases() {
        let dir = tempfile::tempdir().unwrap();
        let integration = write_case(
            dir.path(),
            "portable",
            "type: integration\nname: portable\ntimeout: 1s\nruntimes: [linux, windows]\nprocedure: []\n",
        );
        let correctness = write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        let integration = crate::CASE_LOADER.load_file(&integration).unwrap();
        let correctness = crate::CASE_LOADER.load_file(&correctness).unwrap();
        assert_eq!(integration.len(), 1);
        assert_eq!(correctness.len(), 1);
        assert_eq!(*integration[0].subject_os(), BTreeSet::from([Os::Linux, Os::Windows]));
        for (runtime, eligible) in [
            (LINUX_RUNTIME, true),
            (MAC_RUNTIME, false),
            (WINDOWS_RUNTIME, true),
            ("unknown", false),
        ] {
            assert_eq!(integration[0].is_eligible_for(runtime), eligible, "{runtime}");
            assert!(correctness[0].is_eligible_for(runtime), "{runtime}");
        }
    }

    #[test]
    fn load_file_rejects_unknown_and_non_string_types_and_empty_matrices() {
        let dir = tempfile::tempdir().unwrap();
        for (yaml, expected) in [
            ("type: 3\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: null\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: true\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: []\n".to_string(), vec!["'type' in", "must be a string"]),
            ("type: {}\n".to_string(), vec!["'type' in", "must be a string"]),
            (
                "type: bogus\n".to_string(),
                vec!["Unknown test type 'bogus'", "has no top-level 'type'"],
            ),
            (
                format!("type: correctness_matrix\n{CORRECTNESS_FIELDS}variants: []\n"),
                vec!["has no variants defined"],
            ),
        ] {
            let path = write_case(dir.path(), "invalid", &yaml);
            let error = crate::CASE_LOADER
                .load_file(&path)
                .err()
                .expect("invalid case must fail");
            for expected in expected {
                assert!(error.to_string().contains(expected), "{error}");
            }
        }
    }

    #[test]
    fn an_integration_name_must_match_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n";
        let path = write_case(dir.path(), "directory", yaml);
        let error = crate::CASE_LOADER
            .load_file(&path)
            .err()
            .expect("mismatched name must fail");
        let message = error.to_string();
        assert!(message.contains("name 'alpha'"), "{message}");
        assert!(
            message.contains("does not match its directory 'directory'"),
            "{message}"
        );

        let path = write_case(dir.path(), "alpha", yaml);
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(cases.iter().map(CaseConfig::name).collect::<Vec<_>>(), ["alpha"]);
    }

    #[test]
    fn image_overrides_change_the_built_runner_but_not_the_loaded_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "case", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let entries = ["baseline=base:override".parse().unwrap()];
        let overridden = build_runners(&cases, LINUX_RUNTIME, &entries).unwrap();
        let declared = build_runners(&cases, LINUX_RUNTIME, &[]).unwrap();
        assert_eq!(overridden[0].images()["baseline"], "base:override");
        assert_eq!(declared[0].images()["baseline"], "base:file");
    }

    #[test]
    fn facade_metadata_matches_the_built_runner_for_every_checked_in_case() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        for suite in ["integration", "correctness"] {
            let mut loaded = 0;
            let mut built = 0;
            for entry in fs::read_dir(root.join("test").join(suite).join("cases")).unwrap() {
                let dir = entry.unwrap().path();
                let path = dir.join("config.yaml");
                if !dir.is_dir() || !path.is_file() {
                    continue;
                }
                let cases = crate::CASE_LOADER.load_file(&path).unwrap();
                assert!(!cases.is_empty(), "{}", path.display());
                loaded += cases.len();
                let yaml: serde_json::Value = serde_saphyr::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                for case in cases {
                    if yaml["type"] == "integration" {
                        let declared_os: BTreeSet<_> = match yaml.get("runtimes") {
                            Some(runtimes) => runtimes
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|runtime| os_for_runtime(runtime.as_str().unwrap()).unwrap())
                                .collect(),
                            None => BTreeSet::from([os_for_runtime(default_host_runtime()).unwrap()]),
                        };
                        assert_eq!(*case.subject_os(), declared_os, "{}", path.display());
                    }
                    for runtime in [LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME] {
                        if !case.is_eligible_for(runtime) {
                            continue;
                        }
                        let runners = build_runners(std::slice::from_ref(&case), runtime, &[]).unwrap();
                        let runner = &runners[0];
                        assert_eq!(case.name(), runner.name(), "{} under {runtime}", path.display());
                        assert_eq!(
                            case.suite(),
                            runner.suite().name(),
                            "{} under {runtime}",
                            path.display()
                        );
                        assert_eq!(
                            case.source().parent().unwrap(),
                            runner.case_path(),
                            "{} under {runtime}",
                            path.display()
                        );
                        assert_eq!(case.timeout(), runner.timeout(), "{} under {runtime}", path.display());
                        built += 1;
                    }
                }
            }
            assert!(loaded > 0, "{suite} must contain cases");
            assert!(built > 0, "{suite} must contain eligible cases");
        }
    }

    #[test]
    fn disabled_correctness_cases_still_reject_invalid_environments() {
        let dir = tempfile::tempdir().unwrap();
        for (kind, fields, variants) in [
            (
                "correctness",
                CORRECTNESS_FIELDS.replace(
                    "baseline: {image: 'base:file'}",
                    "baseline: {image: 'base:file', env: {INVALID: true}}",
                ),
                "",
            ),
            (
                "correctness_matrix",
                CORRECTNESS_FIELDS.to_string(),
                "variants: [{name: invalid, env: {INVALID: true}}]\n",
            ),
        ] {
            let path = write_case(
                dir.path(),
                "invalid",
                &format!("type: {kind}\n{fields}disabled: true\n{variants}"),
            );
            let error = crate::CASE_LOADER
                .load_file(&path)
                .err()
                .expect("disabled invalid case must fail");
            assert!(
                format!("{error:?}").contains("environment variable 'INVALID' must be a YAML string"),
                "{error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn correctness_names_use_the_supplied_directory_even_when_it_is_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(
            dir.path(),
            "original",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        std::os::unix::fs::symlink(dir.path().join("original"), dir.path().join("alias")).unwrap();
        let cases = crate::CASE_LOADER
            .load_file(&dir.path().join("alias/config.yaml"))
            .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name(), "alias");
        assert_eq!(cases[0].source(), path.canonicalize().unwrap());
    }

    #[test]
    fn a_runner_reports_the_canonical_case_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_case(dir.path(), "case", &format!("type: correctness\n{CORRECTNESS_FIELDS}"));
        let cases = crate::CASE_LOADER
            .load_file(&dir.path().join("case/../case/config.yaml"))
            .unwrap();
        assert_eq!(cases.len(), 1);
        let case = &cases[0];
        assert_eq!(case.source(), path.canonicalize().unwrap());
        assert_eq!(case.name(), "case");
        let runners = build_runners(&cases, LINUX_RUNTIME, &[]).unwrap();
        assert_eq!(runners[0].case_path(), path.canonicalize().unwrap().parent().unwrap());
    }

    #[test]
    fn every_load_error_names_the_canonical_file() {
        let dir = tempfile::tempdir().unwrap();
        for (yaml, reason) in [
            ("type: [".to_string(), "Failed to parse config file as YAML"),
            ("type: 3\n".to_string(), "must be a string"),
            ("type: bogus\n".to_string(), "Unknown test type 'bogus'"),
            (
                format!("{PARTICIPANT_FIELDS}container: {{files: ['settings.yaml:/etc/settings.yaml']}}\n"),
                "unknown field `container`",
            ),
            (
                format!("{PARTICIPANT_FIELDS}intake: {{enabled: true}}\n"),
                "unknown field `intake`",
            ),
            (
                "type: integration\nname: alpha\ntimeout: nope\nprocedure: []\n".to_string(),
                "Failed to parse configuration file",
            ),
            (
                "type: integration\nname: alpha\ntimeout: 1s\nruntimes: []\nprocedure: []\n".to_string(),
                "empty runtimes list",
            ),
            (
                "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [typo]\nprocedure: []\n".to_string(),
                "unknown runtime 'typo'",
            ),
            (
                format!("type: correctness_matrix\n{CORRECTNESS_FIELDS}variants: []\n"),
                "has no variants defined",
            ),
            (
                "type: integration\nname: mismatch\ntimeout: 1s\nprocedure: []\n".to_string(),
                "does not match its directory",
            ),
        ] {
            let path = write_case(dir.path(), "alpha", &yaml);
            let error = crate::CASE_LOADER
                .load_file(&path)
                .err()
                .expect("invalid case must fail");
            let message = format!("{error:#}");
            assert!(message.contains(reason), "{message}");
            assert!(
                message.contains(&path.canonicalize().unwrap().display().to_string()),
                "{message}"
            );
        }
    }
}
