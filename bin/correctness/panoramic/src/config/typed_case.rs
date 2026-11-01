//! Typed case schemas and the runners built from case plans.

use std::{collections::BTreeSet, path::Path, time::Duration};

use panoramic_core::case::{case_name, deserialize_case, CaseConfig, CaseSchema, TypedCase, TypedCaseLoader};
use panoramic_core::{
    layout::{Os, Realization, RealizationKind},
    plan::CasePlan,
};
use saluki_error::{generic_error, GenericError};

use super::{
    integration_bridge, os_for_runtime, parse_case, IntegrationConfig, MatrixVariants, LINUX_RUNTIME, MAC_RUNTIME,
    WINDOWS_RUNTIME,
};
use crate::{
    correctness::{
        case::CorrectnessTestCase,
        config::{Config as CorrectnessConfig, Runtime},
    },
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

impl TypedConfig {
    /// Returns the realization this case's runner creates: integration cases follow the subject OS,
    /// correctness cases their configured runtime.
    pub(crate) fn realization(&self, subject_os: Os) -> Realization {
        let (os, kind) = match self {
            Self::Integration(_) => match subject_os {
                Os::Linux => (Os::Linux, RealizationKind::LinuxContainer),
                Os::MacOs => (Os::MacOs, RealizationKind::HostProcessGroup),
                Os::Windows => (Os::Windows, RealizationKind::WindowsContainer),
            },
            Self::Correctness(config) => match config.runtime {
                Runtime::Docker => (Os::Linux, RealizationKind::LinuxContainer),
                Runtime::KubernetesInDocker => (Os::Linux, RealizationKind::KubernetesPod),
            },
        };
        Realization { os, kind }
    }
}

/// A case loaded with this binary's typed schemas.
pub(crate) type LoadedCase = CaseConfig<TypedConfig>;

/// A case compiled for its selected subject OS.
pub(crate) type LoadedPlan = CasePlan<TypedConfig>;

/// Returns each participant's realization, or the single realization used by a typed case's runner.
pub(crate) fn realizations(plan: &LoadedPlan) -> Vec<(Option<&str>, Realization)> {
    match plan.case().schema() {
        CaseSchema::Participants { .. } => plan
            .layout()
            .expect("a participant plan always has a layout")
            .iter()
            .map(|(name, realization)| (Some(name), realization))
            .collect(),
        CaseSchema::Typed(config) => vec![(None, config.realization(plan.subject_os()))],
    }
}

/// Whether the case needs Kubernetes pods and their cluster.
pub(crate) fn needs_pod(plan: &LoadedPlan) -> bool {
    realizations(plan)
        .iter()
        .any(|(_, realization)| realization.kind == RealizationKind::KubernetesPod)
}

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

/// Participant translation and runner construction for case plans.
pub(crate) trait RunnableCase {
    /// Translates a participant case into the configuration its integration runner receives.
    ///
    /// # Errors
    ///
    /// Returns an error naming the case and source if it uses another schema or suite, has unsupported
    /// participant settings or steps.
    /// See [`integration_bridge::integration_config`].
    fn integration_config(&self) -> Result<IntegrationConfig, GenericError>;

    /// Builds a runner from a copy of the case data, applying image choices to that copy.
    ///
    /// # Errors
    ///
    /// Participant cases propagate [`Self::integration_config`] errors.
    fn build_test(
        &self, settings: &IntegrationSettings, overrides: &ImageOverrides<'_>,
    ) -> Result<Box<dyn Test>, GenericError>;
}

impl RunnableCase for LoadedPlan {
    fn integration_config(&self) -> Result<IntegrationConfig, GenericError> {
        let case = self.case();
        let CaseSchema::Participants { file, .. } = case.schema() else {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: integration translation requires a case that declares participants",
                case.name(),
                case.source().display()
            ));
        };
        if case.suite() != TestSuite::Integration.name() {
            return Err(generic_error!(
                "Cannot run case '{}' from {}: the integration runner runs only integration-suite cases",
                case.name(),
                case.source().display()
            ));
        }
        integration_bridge::integration_config(
            file,
            case.name(),
            case.source(),
            |participant, fact| self.fact(participant, fact),
            |participant| self.effective_config(participant),
        )
        .map_err(|reason| {
            generic_error!(
                "Cannot run case '{}' from {}: {}",
                case.name(),
                case.source().display(),
                reason
            )
        })
    }

    fn build_test(
        &self, settings: &IntegrationSettings, overrides: &ImageOverrides<'_>,
    ) -> Result<Box<dyn Test>, GenericError> {
        match self.case().schema() {
            CaseSchema::Typed(TypedConfig::Integration(config)) => {
                Ok(Box::new(IntegrationTestCase::new(config.clone(), settings)))
            }
            CaseSchema::Typed(TypedConfig::Correctness(config)) => {
                let mut config = config.clone();
                overrides.apply_correctness(&mut config);
                Ok(Box::new(CorrectnessTestCase::new(
                    self.case().name().to_string(),
                    config,
                )))
            }
            CaseSchema::Participants { .. } => {
                Ok(Box::new(IntegrationTestCase::new(self.integration_config()?, settings)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::case::{CaseLoader, Registries};

    use super::*;
    use crate::config::{
        build_runners, discover_in_scope,
        tests::{build_case_runners, plan, write_case, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
    };

    #[test]
    fn a_participant_case_needs_the_realization_of_every_participant_in_its_layout() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "  adp:",
            "  zebra: {type: agent-adp}\n  alpha: {type: agent-adp, os: linux}\n  adp:",
        );
        let plan = plan(&yaml, Os::MacOs);
        assert_eq!(
            realizations(&plan),
            vec![
                (
                    Some("adp"),
                    Realization {
                        os: Os::MacOs,
                        kind: RealizationKind::HostProcessGroup
                    }
                ),
                (
                    Some("alpha"),
                    Realization {
                        os: Os::Linux,
                        kind: RealizationKind::LinuxContainer
                    }
                ),
                (
                    Some("zebra"),
                    Realization {
                        os: Os::MacOs,
                        kind: RealizationKind::HostProcessGroup
                    }
                ),
            ]
        );
        assert!(!needs_pod(&plan));
    }

    #[test]
    fn typed_cases_need_the_realization_their_runner_creates() {
        for (os, kind) in [
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ] {
            let plan = plan(
                "type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, mac, windows]\nprocedure: []\n",
                os,
            );
            assert_eq!(realizations(&plan), [(None, Realization { os, kind })]);
            assert!(!needs_pod(&plan));
        }
        for (runtime, kind) in [
            ("docker", RealizationKind::LinuxContainer),
            ("kubernetes_in_docker", RealizationKind::KubernetesPod),
        ] {
            let plan = plan(
                &format!(
                    "type: correctness\n{}",
                    CORRECTNESS_FIELDS.replace("runtime: docker", &format!("runtime: {runtime}"))
                ),
                Os::Linux,
            );
            assert_eq!(realizations(&plan), [(None, Realization { os: Os::Linux, kind })]);
            assert_eq!(needs_pod(&plan), kind == RealizationKind::KubernetesPod);
        }
    }

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
            let plan = CasePlan::compile(cases.into_iter().next().unwrap(), Os::Linux, &crate::REGISTRIES)
                .unwrap()
                .unwrap();
            assert_eq!(
                plan.integration_config().unwrap_err().to_string(),
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
                participant_types: panoramic_participants::REGISTERED_TYPES,
                steps: panoramic_steps::REGISTERED_STEPS,
            },
            TypedConfigLoader,
            TestSuite::Correctness.name(),
        );
        let cases = loader.load_file(&path).unwrap();
        let error = build_case_runners(cases, Os::Linux, &[])
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
    fn selection_plans_participant_and_integration_cases_only_for_their_subject_os() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "participant",
            &PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]"),
        );
        write_case(
            dir.path(),
            "integration",
            "type: integration\nname: integration\ntimeout: 1s\nruntimes: [linux, windows]\nprocedure: []\n",
        );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plans = discover_in_scope(&[dir.path().to_path_buf()], os).unwrap();
            let names: Vec<_> = plans.iter().map(|plan| plan.case().name()).collect();
            assert_eq!(
                names,
                if os == Os::MacOs {
                    vec![]
                } else {
                    vec!["integration", "participant"]
                }
            );
            for plan in plans {
                assert_eq!(plan.subject_os(), os);
                assert_eq!(plan.layout().is_some(), plan.case().name() == "participant");
            }
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
        let runners = build_case_runners(cases, Os::Linux, &[]).unwrap();
        for (built, variant) in runners.iter().zip(["second", "first"]) {
            let case = built.plan.case();
            assert_eq!(case.source(), source);
            assert_eq!(case.timeout(), CORRECTNESS_TIMEOUT);
            assert_eq!(case.suite(), TestSuite::Correctness.name());
            assert_eq!(case.description(), None);
            assert_eq!(*case.subject_os(), BTreeSet::from([Os::Linux]));
            assert!(!case.is_disabled());
            assert_eq!(case.name(), format!("matrix/{variant}"));
            assert_eq!(case.source().parent().unwrap(), source.parent().unwrap());

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
    fn disabled_cases_are_never_planned() {
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
            }
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                assert!(
                    discover_in_scope(&[dir.path().to_path_buf()], os).unwrap().is_empty(),
                    "{kind} under {os:?}"
                );
            }
        }
    }

    #[test]
    fn correctness_cases_are_planned_for_linux_under_every_selected_os() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "correctness",
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
        );
        write_case(
            dir.path(),
            "matrix",
            &format!("type: correctness_matrix\n{CORRECTNESS_FIELDS}variants: [{{name: first}}, {{name: second}}]\n"),
        );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plans = discover_in_scope(&[dir.path().to_path_buf()], os).unwrap();
            assert_eq!(
                plans.iter().map(|plan| plan.case().name()).collect::<Vec<_>>(),
                ["correctness", "matrix/first", "matrix/second"]
            );
            for plan in &plans {
                assert_eq!(plan.subject_os(), Os::Linux);
                assert!(plan.layout().is_none());
            }
            let runners = build_runners(plans, os, &[]).unwrap();
            assert!(runners.iter().all(|built| built.runner.runtime() == "docker"));
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
        let plans: Vec<_> = cases
            .into_iter()
            .map(|case| CasePlan::compile(case, Os::Linux, &crate::REGISTRIES).unwrap().unwrap())
            .collect();
        let overridden = build_runners(&plans, Os::Linux, &entries).unwrap();
        let declared = build_runners(&plans, Os::Linux, &[]).unwrap();
        assert_eq!(overridden[0].runner.images()["baseline"], "base:override");
        assert_eq!(declared[0].runner.images()["baseline"], "base:file");
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
        let runners = build_case_runners(cases, Os::Linux, &[]).unwrap();
        assert_eq!(
            runners[0].plan.case().source().parent().unwrap(),
            path.canonicalize().unwrap().parent().unwrap()
        );
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
