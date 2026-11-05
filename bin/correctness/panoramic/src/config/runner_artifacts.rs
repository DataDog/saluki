//! Maps artifact choices onto the correctness runner's settings.

use std::collections::BTreeMap;

use panoramic_core::{
    artifact::{ArtifactBinding, ArtifactRole},
    backend::FIXUP_IMAGE,
};
use panoramic_participants::{AgentAdp, TOOLS_LINUX_IMAGE};
use saluki_error::{generic_error, GenericError};

use crate::{
    correctness::{
        case::{
            BASELINE_IMAGE_NAME, COMPARISON_IMAGE_NAME, INTAKE_IMAGE_NAME as DATADOG_INTAKE_IMAGE_NAME,
            MILLSTONE_IMAGE_NAME,
        },
        config::Config as CorrectnessConfig,
    },
    image_override::{ImageOverride, ImageOverrides},
    test::{RunnerSettings, Test},
};

/// Where the correctness runner takes its images from.
pub(crate) enum RunnerArtifacts<'a> {
    /// Case files and harness defaults, replaced by image overrides.
    Overrides(&'a [ImageOverride]),
    /// References bound to artifact roles.
    Bound(BoundArtifacts),
}

/// Artifact inputs given explicitly through legacy flags or their environment variables.
#[cfg_attr(test, derive(Default))]
pub(crate) struct LegacyArtifactInputs<'a> {
    pub(crate) image_overrides: &'a [ImageOverride],
    pub(crate) alpine_image: Option<&'a str>,
}

/// Needed artifact bindings, keyed by role name.
pub(crate) struct BoundArtifacts {
    bindings: BTreeMap<&'static str, ArtifactBinding>,
}

impl BoundArtifacts {
    /// Indexes bindings collected from every case in scope.
    pub(crate) fn new(bindings: Vec<ArtifactBinding>) -> Self {
        Self {
            bindings: bindings
                .into_iter()
                .map(|binding| (binding.role.name, binding))
                .collect(),
        }
    }

    fn reference(&self, role: ArtifactRole) -> Option<&str> {
        self.bindings.get(role.name).map(|binding| binding.reference.as_str())
    }

    /// Checks override names, then compares explicit inputs with the bindings of needed roles.
    pub(crate) fn check_legacy_inputs(
        &self, inputs: &LegacyArtifactInputs<'_>, runners: &[&dyn Test],
    ) -> Result<(), GenericError> {
        ImageOverrides::new(inputs.image_overrides).validate(runners)?;

        let mut disagreements = Vec::new();
        let mut check_image = |flag: &str, image: &str, role: Option<ArtifactRole>| {
            let Some(role) = role else {
                disagreements.push(format!("{flag} names no artifact role"));
                return;
            };
            if let Some(binding) = self.bindings.get(role.name) {
                if image != binding.reference {
                    disagreements.push(format!(
                        "{flag} names '{image}' for role '{}', bound to '{}' ({})",
                        role.name,
                        binding.reference,
                        binding.source.name()
                    ));
                }
            }
        };
        if let Some(image) = inputs.alpine_image {
            check_image("--alpine-image (PANORAMIC_ALPINE_IMAGE)", image, Some(FIXUP_IMAGE));
        }
        for entry in inputs.image_overrides {
            let role = match entry.name.as_str() {
                DATADOG_INTAKE_IMAGE_NAME | MILLSTONE_IMAGE_NAME => Some(TOOLS_LINUX_IMAGE),
                BASELINE_IMAGE_NAME | COMPARISON_IMAGE_NAME => Some(AgentAdp::LINUX_IMAGE),
                _ => None,
            };
            check_image(
                &format!("--image-override {} (PANORAMIC_IMAGE_OVERRIDES)", entry.name),
                &entry.image,
                role,
            );
        }
        if disagreements.is_empty() {
            Ok(())
        } else {
            Err(generic_error!(
                "Legacy artifact inputs disagree with the --artifacts bindings:\n  - {}",
                disagreements.join("\n  - ")
            ))
        }
    }

    /// Applies the bound fix-up image when a case needs it.
    pub(crate) fn apply_runner_settings(&self, settings: &mut RunnerSettings) {
        if let Some(reference) = self.reference(FIXUP_IMAGE) {
            settings.alpine_image = reference.to_string();
        }
    }
}

impl RunnerArtifacts<'_> {
    /// Applies image overrides or role bindings to correctness containers, preserving other images.
    pub(crate) fn apply_correctness(&self, config: &mut CorrectnessConfig) {
        match self {
            Self::Overrides(entries) => ImageOverrides::new(entries).apply_correctness(config),
            Self::Bound(bound) => {
                if let Some(reference) = bound.reference(AgentAdp::LINUX_IMAGE) {
                    config.baseline.image = reference.to_string();
                    config.comparison.image = reference.to_string();
                }
                if let Some(reference) = bound.reference(TOOLS_LINUX_IMAGE) {
                    config.datadog_intake.image = reference.to_string();
                    config.millstone.image = reference.to_string();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser as _;
    use panoramic_core::layout::Os;
    use panoramic_core::{
        artifact::ArtifactManifest,
        case::CaseSchema,
        plan::{needed_artifacts, Planner},
    };

    use super::*;
    use crate::{
        cli::{Cli, Command},
        config::{
            build_runners,
            tests::CORRECTNESS_FIELDS,
            typed_case::{LoadedPlan, TypedConfig},
        },
    };

    fn plans(yaml: &str, os: Os, manifest: bool) -> Vec<LoadedPlan> {
        let manifest = if manifest {
            ArtifactManifest::new(
                "test",
                BTreeMap::from([
                    (AgentAdp::LINUX_IMAGE.name.into(), "target:linux".into()),
                    (AgentAdp::WINDOWS_IMAGE.name.into(), "target:windows".into()),
                    (TOOLS_LINUX_IMAGE.name.into(), "tools:bound".into()),
                    (AgentAdp::ADP_HOST_BINARY.name.into(), "build/adp".into()),
                    (AgentAdp::AGENT_HOST_INSTALL.name.into(), "build/install".into()),
                    (FIXUP_IMAGE.name.into(), "fixup:bound".into()),
                ]),
            )
            .unwrap()
        } else {
            ArtifactManifest::default()
        };
        let mut planner = Planner::new(crate::REGISTRIES, manifest).unwrap();
        crate::CASE_LOADER
            .load_content(yaml, "case", Path::new("case/config.yaml"))
            .unwrap()
            .into_iter()
            .map(|case| planner.compile(case, os).unwrap().into_plan().unwrap())
            .collect()
    }

    fn default_settings() -> RunnerSettings {
        let Command::Run(cmd) = Cli::try_parse_from(["panoramic", "run"]).unwrap().command else {
            panic!("expected run")
        };
        cmd.runner_settings()
    }

    const CORRECTNESS_CASE: &str = "type: correctness\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n";

    fn check_inputs(plans: &[LoadedPlan], inputs: &LegacyArtifactInputs<'_>) -> Result<(), GenericError> {
        let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(plans)));
        let built = build_runners(plans, &artifacts).unwrap();
        let RunnerArtifacts::Bound(bound) = artifacts else {
            unreachable!()
        };
        let runners: Vec<_> = built.iter().map(|built| built.runner().unwrap()).collect();
        bound.check_legacy_inputs(inputs, &runners)
    }

    #[test]
    fn a_legacy_input_that_names_another_reference_is_reported_with_its_flag_role_binding_and_source() {
        for manifest in [false, true] {
            let plans = plans(CORRECTNESS_CASE, Os::Linux, manifest);
            let inputs = LegacyArtifactInputs {
                image_overrides: &[],
                alpine_image: Some("other:alpine"),
            };
            let error = check_inputs(&plans, &inputs).unwrap_err().to_string();
            let reference = if manifest {
                "fixup:bound"
            } else {
                FIXUP_IMAGE.local_default
            };
            let source = if manifest { "manifest" } else { "local default" };
            assert_eq!(error, format!(
                "Legacy artifact inputs disagree with the --artifacts bindings:\n  - --alpine-image (PANORAMIC_ALPINE_IMAGE) names 'other:alpine' for role 'fixup-image', bound to '{reference}' ({source})"
            ));
        }
    }

    #[test]
    fn two_overrides_for_one_role_with_different_images_cannot_both_agree() {
        let plans = plans(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux, true);
        for (intake, millstone, name) in [
            ("tools:bound", "other:tools", "millstone"),
            ("other:tools", "tools:bound", "datadog-intake"),
        ] {
            let overrides = [
                format!("datadog-intake={intake}").parse().unwrap(),
                format!("millstone={millstone}").parse().unwrap(),
            ];
            let inputs = LegacyArtifactInputs {
                image_overrides: &overrides,
                ..Default::default()
            };
            let error = check_inputs(&plans, &inputs).unwrap_err().to_string();
            assert_eq!(error, format!(
                "Legacy artifact inputs disagree with the --artifacts bindings:\n  - --image-override {name} (PANORAMIC_IMAGE_OVERRIDES) names 'other:tools' for role 'tools-linux-image', bound to 'tools:bound' (manifest)"
            ));
        }
    }

    #[test]
    fn image_override_names_are_still_validated_under_run_artifacts() {
        let plans = plans(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux, true);
        for entries in [
            vec!["baseline=x", "baseline=y"],
            vec!["unknown=x"],
            vec!["container=x"],
            vec!["intake=x"],
            vec!["unknown=x", "unknown=y"],
            vec!["baseline=x", "unknown=y", "baseline=z"],
            vec!["baseline=x", "baseline=y", "unknown=z"],
        ] {
            let overrides: Vec<_> = entries.into_iter().map(|entry| entry.parse().unwrap()).collect();
            let expected = build_runners(&plans, &RunnerArtifacts::Overrides(&overrides))
                .err()
                .unwrap()
                .to_string();
            let inputs = LegacyArtifactInputs {
                image_overrides: &overrides,
                alpine_image: Some("other:alpine"),
            };
            assert_eq!(check_inputs(&plans, &inputs).unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn ci_correctness_overrides_agree_with_a_manifest_binding_the_same_images() {
        let plans = plans(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux, true);
        let overrides = [
            "millstone=tools:bound",
            "datadog-intake=tools:bound",
            "comparison=target:linux",
            "baseline=target:linux",
        ]
        .map(|entry| entry.parse().unwrap());
        let inputs = LegacyArtifactInputs {
            image_overrides: &overrides,
            alpine_image: Some("fixup:bound"),
        };
        let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
        let built = build_runners(&plans, &artifacts).unwrap();
        let RunnerArtifacts::Bound(bound) = artifacts else {
            unreachable!()
        };
        bound
            .check_legacy_inputs(&inputs, &[built[0].runner().unwrap()])
            .unwrap();
        assert_eq!(
            built[0].runner().unwrap().images(),
            BTreeMap::from([
                ("millstone", "tools:bound".into()),
                ("datadog-intake", "tools:bound".into()),
                ("comparison", "target:linux".into()),
                ("baseline", "target:linux".into()),
            ])
        );
        let mut settings = default_settings();
        bound.apply_runner_settings(&mut settings);
        assert_eq!(settings.alpine_image, "fixup:bound");
    }

    #[test]
    fn bound_correctness_runners_replace_every_file_image_with_role_references() {
        for manifest in [false, true] {
            for runtime in ["docker", "kubernetes_in_docker"] {
                let yaml = format!("type: correctness\n{}datadog_intake: {{image: 'intake:file'}}\nmillstone: {{image: 'millstone:file'}}\n",
                    CORRECTNESS_FIELDS.replace("runtime: docker", &format!("runtime: {runtime}")));
                let plans = plans(&yaml, Os::Linux, manifest);
                let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
                let runners = build_runners(&plans, &artifacts).unwrap();
                let target = if manifest {
                    "target:linux"
                } else {
                    AgentAdp::LINUX_IMAGE.local_default
                };
                let tools = if manifest {
                    "tools:bound"
                } else {
                    TOOLS_LINUX_IMAGE.local_default
                };
                assert_eq!(
                    runners[0].runner().unwrap().images(),
                    BTreeMap::from([
                        ("baseline", target.into()),
                        ("comparison", target.into()),
                        ("datadog-intake", tools.into()),
                        ("millstone", tools.into()),
                    ])
                );
                let CaseSchema::Typed(TypedConfig::Correctness(config)) = plans[0].case().schema() else {
                    panic!("expected correctness")
                };
                assert_eq!(config.baseline.image, "base:file");
                assert_eq!(config.comparison.image, "comp:file");
                assert_eq!(config.datadog_intake.image, "intake:file");
                assert_eq!(config.millstone.image, "millstone:file");
            }
        }
    }

    #[test]
    fn override_runner_artifacts_build_the_same_runners_as_before() {
        let plans = plans(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux, true);
        let entries = [
            "baseline=base:override".parse().unwrap(),
            "millstone=millstone:override".parse().unwrap(),
        ];
        let runners = build_runners(&plans, &RunnerArtifacts::Overrides(&entries)).unwrap();
        assert_eq!(
            runners[0].runner().unwrap().images(),
            BTreeMap::from([
                ("baseline", "base:override".into()),
                ("comparison", "comp:file".into()),
                ("datadog-intake", TOOLS_LINUX_IMAGE.local_default.into()),
                ("millstone", "millstone:override".into()),
            ])
        );
        let defaults = build_runners(&plans, &RunnerArtifacts::Overrides(&[])).unwrap();
        assert_eq!(defaults[0].runner().unwrap().images()["baseline"], "base:file");
        for entries in [
            vec!["unused=x".parse().unwrap()],
            vec!["baseline=x".parse().unwrap(), "baseline=y".parse().unwrap()],
        ] {
            assert!(build_runners(&plans, &RunnerArtifacts::Overrides(&entries)).is_err());
        }
    }
}
