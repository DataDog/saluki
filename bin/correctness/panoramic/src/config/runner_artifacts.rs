//! Maps artifact choices onto the integration and correctness runners' settings.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use panoramic_core::{
    artifact::{ArtifactBinding, ArtifactKind, ArtifactRole},
    backend::FIXUP_IMAGE,
    layout::Os,
};
use panoramic_participants::{AgentAdp, TOOLS_LINUX_IMAGE};
use saluki_error::{generic_error, GenericError};

use super::{runtime_for_os, INTAKE_IMAGE_NAME, LINUX_RUNTIME, TARGET_IMAGE_NAME, WINDOWS_RUNTIME};
use crate::{
    correctness::{
        case::{
            BASELINE_IMAGE_NAME, COMPARISON_IMAGE_NAME, INTAKE_IMAGE_NAME as DATADOG_INTAKE_IMAGE_NAME,
            MILLSTONE_IMAGE_NAME,
        },
        config::Config as CorrectnessConfig,
    },
    image_override::{ImageOverride, ImageOverrides},
    integration::IntegrationSettings,
    test::{RunnerSettings, Test},
};

/// Where the integration and correctness runners take their images and binaries from.
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
    pub(crate) adp_binary_path: Option<&'a Path>,
    pub(crate) core_agent_binary_path: Option<&'a Path>,
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
        &self, inputs: &LegacyArtifactInputs<'_>, subject_os: Os, runners: &[&dyn Test],
    ) -> Result<(), GenericError> {
        ImageOverrides::new(inputs.image_overrides).validate(runners)?;

        let mut disagreements = Vec::new();
        for (flag, path, role, expected_path) in [
            (
                "--adp-binary-path (PANORAMIC_ADP_BINARY_PATH)",
                inputs.adp_binary_path,
                AgentAdp::ADP_HOST_BINARY,
                (|reference: &str| PathBuf::from(reference)) as fn(&str) -> PathBuf,
            ),
            (
                "--core-agent-binary-path (PANORAMIC_CORE_AGENT_BINARY_PATH)",
                inputs.core_agent_binary_path,
                AgentAdp::AGENT_HOST_INSTALL,
                |reference| core_agent_binary_path(Path::new(reference)),
            ),
        ] {
            let (Some(path), Some(binding)) = (path, self.bindings.get(role.name)) else {
                continue;
            };
            let expected = expected_path(&binding.reference);
            if path != expected {
                let mut message = format!(
                    "{flag} names '{}' for role '{}', bound to '{}' ({})",
                    path.display(),
                    role.name,
                    binding.reference,
                    binding.source.name()
                );
                if role == AgentAdp::AGENT_HOST_INSTALL {
                    message.push_str(&format!(", whose main binary is '{}'", expected.display()));
                }
                disagreements.push(message);
            }
        }

        let mut check_image = |flag: &str, image: &str, role: Option<ArtifactRole>| {
            let Some(role) = role else {
                disagreements.push(format!("{flag} names no artifact role on {}", subject_os.name()));
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
                TARGET_IMAGE_NAME => target_role_for_runtime(runtime_for_os(subject_os)),
                INTAKE_IMAGE_NAME | DATADOG_INTAKE_IMAGE_NAME | MILLSTONE_IMAGE_NAME => Some(TOOLS_LINUX_IMAGE),
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

    /// Applies bound host binaries and the fix-up image, preserving settings for unneeded roles.
    pub(crate) fn apply_runner_settings(&self, settings: &mut RunnerSettings) {
        if let Some(reference) = self.reference(AgentAdp::ADP_HOST_BINARY) {
            settings.adp_binary_path = PathBuf::from(reference);
        }
        if let Some(reference) = self.reference(AgentAdp::AGENT_HOST_INSTALL) {
            settings.core_agent_binary_path = core_agent_binary_path(Path::new(reference));
        }
        if let Some(reference) = self.reference(FIXUP_IMAGE) {
            settings.alpine_image = reference.to_string();
        }
    }
}

impl RunnerArtifacts<'_> {
    /// Builds runtime and image settings from overrides or role bindings.
    pub(crate) fn integration_settings(&self, runtime: &str) -> IntegrationSettings {
        let target_role = target_role_for_runtime(runtime);
        let (target_image, intake_image) = match self {
            Self::Overrides(entries) => {
                let overrides = ImageOverrides::new(entries);
                (
                    target_role.map(|role| overrides.image(TARGET_IMAGE_NAME, role.local_default)),
                    overrides.image(INTAKE_IMAGE_NAME, TOOLS_LINUX_IMAGE.local_default),
                )
            }
            Self::Bound(bound) => (
                target_role.map(|role| bound.reference(role).unwrap_or(role.local_default).to_string()),
                bound
                    .reference(TOOLS_LINUX_IMAGE)
                    .unwrap_or(TOOLS_LINUX_IMAGE.local_default)
                    .to_string(),
            ),
        };
        IntegrationSettings {
            runtime: runtime.to_string(),
            target_image,
            intake_image,
        }
    }

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

fn target_role_for_runtime(runtime: &str) -> Option<ArtifactRole> {
    match runtime {
        LINUX_RUNTIME => Some(AgentAdp::LINUX_IMAGE),
        WINDOWS_RUNTIME => Some(AgentAdp::WINDOWS_IMAGE),
        _ => None,
    }
}

/// Joins the install directory with the Core Agent role's main binary path.
pub(crate) fn core_agent_binary_path(install: &Path) -> PathBuf {
    let ArtifactKind::Install { main_binary } = AgentAdp::AGENT_HOST_INSTALL.kind else {
        unreachable!("the Core Agent role declares an install");
    };
    install.join(main_binary)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use panoramic_core::{
        artifact::ArtifactManifest,
        case::CaseSchema,
        layout::Os,
        plan::{needed_artifacts, Planner},
    };

    use super::*;
    use crate::{
        cli::{Cli, Command},
        config::{
            build_runners, runtime_for_os,
            tests::CORRECTNESS_FIELDS,
            typed_case::{LoadedPlan, TypedConfig},
        },
        integration::IntegrationTestCase,
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

    const INTEGRATION_CASE: &str = "type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, windows, mac]\nintake: {enabled: true}\nprocedure: []\n";

    fn check_inputs(plans: &[LoadedPlan], os: Os, inputs: &LegacyArtifactInputs<'_>) -> Result<(), GenericError> {
        let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(plans)));
        let built = build_runners(plans, os, &artifacts).unwrap();
        let RunnerArtifacts::Bound(bound) = artifacts else {
            unreachable!()
        };
        let runners: Vec<_> = built.iter().map(|built| built.runner().unwrap()).collect();
        bound.check_legacy_inputs(inputs, os, &runners)
    }

    #[test]
    fn each_legacy_input_that_names_its_roles_binding_agrees() {
        let correctness = format!("type: correctness\n{CORRECTNESS_FIELDS}");
        for (os, yaml, flag, name, role, reference) in [
            (
                Os::Linux,
                INTEGRATION_CASE,
                "--image-override",
                "container=",
                AgentAdp::LINUX_IMAGE,
                "target:linux",
            ),
            (
                Os::Windows,
                INTEGRATION_CASE,
                "--image-override",
                "container=",
                AgentAdp::WINDOWS_IMAGE,
                "target:windows",
            ),
            (
                Os::Linux,
                INTEGRATION_CASE,
                "--image-override",
                "intake=",
                TOOLS_LINUX_IMAGE,
                "tools:bound",
            ),
            (
                Os::Linux,
                correctness.as_str(),
                "--image-override",
                "datadog-intake=",
                TOOLS_LINUX_IMAGE,
                "tools:bound",
            ),
            (
                Os::Linux,
                correctness.as_str(),
                "--image-override",
                "millstone=",
                TOOLS_LINUX_IMAGE,
                "tools:bound",
            ),
            (
                Os::Linux,
                correctness.as_str(),
                "--image-override",
                "baseline=",
                AgentAdp::LINUX_IMAGE,
                "target:linux",
            ),
            (
                Os::Linux,
                correctness.as_str(),
                "--image-override",
                "comparison=",
                AgentAdp::LINUX_IMAGE,
                "target:linux",
            ),
            (
                Os::Linux,
                INTEGRATION_CASE,
                "--alpine-image",
                "",
                FIXUP_IMAGE,
                "fixup:bound",
            ),
            (
                Os::MacOs,
                INTEGRATION_CASE,
                "--adp-binary-path",
                "",
                AgentAdp::ADP_HOST_BINARY,
                "build/adp",
            ),
            (
                Os::MacOs,
                INTEGRATION_CASE,
                "--core-agent-binary-path",
                "",
                AgentAdp::AGENT_HOST_INSTALL,
                "build/install",
            ),
        ] {
            for manifest in [false, true] {
                let plans = plans(yaml, os, manifest);
                let bound_reference = if manifest { reference } else { role.local_default };
                let reference = if matches!(role.kind, ArtifactKind::Install { .. }) {
                    core_agent_binary_path(Path::new(bound_reference)).display().to_string()
                } else {
                    bound_reference.to_string()
                };
                let Command::Run(cmd) = Cli::try_parse_from(["panoramic", "run", flag, &format!("{name}{reference}")])
                    .unwrap()
                    .command
                else {
                    panic!("expected run")
                };
                check_inputs(&plans, os, &cmd.legacy_artifact_inputs()).unwrap();

                let Command::Run(cmd) = Cli::try_parse_from(["panoramic", "run", flag, &format!("{name}different")])
                    .unwrap()
                    .command
                else {
                    panic!("expected run")
                };
                let label = match flag {
                    "--adp-binary-path" => "--adp-binary-path (PANORAMIC_ADP_BINARY_PATH)".to_string(),
                    "--core-agent-binary-path" => {
                        "--core-agent-binary-path (PANORAMIC_CORE_AGENT_BINARY_PATH)".to_string()
                    }
                    "--alpine-image" => "--alpine-image (PANORAMIC_ALPINE_IMAGE)".to_string(),
                    _ => format!(
                        "--image-override {} (PANORAMIC_IMAGE_OVERRIDES)",
                        name.trim_end_matches('=')
                    ),
                };
                let source = if manifest { "manifest" } else { "local default" };
                let suffix = if matches!(role.kind, ArtifactKind::Install { .. }) {
                    format!(", whose main binary is '{reference}'")
                } else {
                    String::new()
                };
                assert_eq!(check_inputs(&plans, os, &cmd.legacy_artifact_inputs()).unwrap_err().to_string(), format!(
                    "Legacy artifact inputs disagree with the --artifacts bindings:\n  - {label} names 'different' for role '{}', bound to '{bound_reference}' ({source}){suffix}", role.name
                ));
            }
        }
    }

    #[test]
    fn a_legacy_input_that_names_another_reference_is_reported_with_its_flag_role_binding_and_source() {
        for manifest in [false, true] {
            let plans = plans(INTEGRATION_CASE, Os::Linux, manifest);
            let inputs = LegacyArtifactInputs {
                alpine_image: Some("other:alpine"),
                ..Default::default()
            };
            let error = check_inputs(&plans, Os::Linux, &inputs).unwrap_err().to_string();
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
    fn every_disagreement_is_reported_at_once_in_flag_then_override_order() {
        let mut plans = plans(INTEGRATION_CASE, Os::MacOs, true);
        plans.extend(self::plans(
            &format!("type: correctness\n{CORRECTNESS_FIELDS}"),
            Os::Linux,
            false,
        ));
        let overrides = [
            "comparison=x".parse().unwrap(),
            "millstone=y".parse().unwrap(),
            "baseline=z".parse().unwrap(),
        ];
        let inputs = LegacyArtifactInputs {
            image_overrides: &overrides,
            adp_binary_path: Some(Path::new("other/adp")),
            core_agent_binary_path: Some(Path::new("other/bin/agent/agent")),
            alpine_image: Some("other:alpine"),
        };
        let error = check_inputs(&plans, Os::Linux, &inputs).unwrap_err().to_string();
        let main_binary = Path::new("build/install").join("bin/agent/agent");
        assert_eq!(error, format!(concat!(
            "Legacy artifact inputs disagree with the --artifacts bindings:\n",
            "  - --adp-binary-path (PANORAMIC_ADP_BINARY_PATH) names 'other/adp' for role 'adp-host-binary', bound to 'build/adp' (manifest)\n",
            "  - --core-agent-binary-path (PANORAMIC_CORE_AGENT_BINARY_PATH) names 'other/bin/agent/agent' for role 'agent-host-install', bound to 'build/install' (manifest), whose main binary is '{}'\n",
            "  - --alpine-image (PANORAMIC_ALPINE_IMAGE) names 'other:alpine' for role 'fixup-image', bound to 'alpine:latest' (local default)\n",
            "  - --image-override comparison (PANORAMIC_IMAGE_OVERRIDES) names 'x' for role 'agent-adp-linux-image', bound to 'saluki-images/datadog-agent:testing-devel' (local default)\n",
            "  - --image-override millstone (PANORAMIC_IMAGE_OVERRIDES) names 'y' for role 'tools-linux-image', bound to 'saluki-images/correctness-tools:latest' (local default)\n",
            "  - --image-override baseline (PANORAMIC_IMAGE_OVERRIDES) names 'z' for role 'agent-adp-linux-image', bound to 'saluki-images/datadog-agent:testing-devel' (local default)"
        ), main_binary.display()));
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
            let error = check_inputs(&plans, Os::Linux, &inputs).unwrap_err().to_string();
            assert_eq!(error, format!(
                "Legacy artifact inputs disagree with the --artifacts bindings:\n  - --image-override {name} (PANORAMIC_IMAGE_OVERRIDES) names 'other:tools' for role 'tools-linux-image', bound to 'tools:bound' (manifest)"
            ));
        }
    }

    #[test]
    fn the_core_agent_path_agrees_when_it_is_the_bound_install_joined_with_its_main_binary() {
        let plans = plans(INTEGRATION_CASE, Os::MacOs, true);
        let mut inputs = LegacyArtifactInputs {
            core_agent_binary_path: Some(Path::new("build/install/bin/agent/agent")),
            ..Default::default()
        };
        check_inputs(&plans, Os::MacOs, &inputs).unwrap();
        inputs.core_agent_binary_path = Some(Path::new("build/install"));
        let main_binary = Path::new("build/install").join("bin/agent/agent");
        assert_eq!(check_inputs(&plans, Os::MacOs, &inputs).unwrap_err().to_string(), format!(
            "Legacy artifact inputs disagree with the --artifacts bindings:\n  - --core-agent-binary-path (PANORAMIC_CORE_AGENT_BINARY_PATH) names 'build/install' for role 'agent-host-install', bound to 'build/install' (manifest), whose main binary is '{}'",
            main_binary.display()
        ));
    }

    #[test]
    fn references_are_compared_without_resolving_paths_or_image_tags() {
        let linux = plans(INTEGRATION_CASE, Os::Linux, false);
        let inputs = LegacyArtifactInputs {
            alpine_image: Some("alpine"),
            ..Default::default()
        };
        assert!(check_inputs(&linux, Os::Linux, &inputs).is_err());
        let macos = plans(INTEGRATION_CASE, Os::MacOs, true);
        let inputs = LegacyArtifactInputs {
            adp_binary_path: Some(Path::new("./build/adp")),
            ..Default::default()
        };
        assert!(check_inputs(&macos, Os::MacOs, &inputs).is_err());
        for path in ["build/adp/", "build//adp", "build/./adp"] {
            let inputs = LegacyArtifactInputs {
                adp_binary_path: Some(Path::new(path)),
                ..Default::default()
            };
            check_inputs(&macos, Os::MacOs, &inputs).unwrap();
        }
    }

    #[test]
    fn a_legacy_input_for_a_role_no_case_needs_is_ignored() {
        let overrides = ["intake=other:tools".parse().unwrap()];
        for (os, inputs) in [
            (
                Os::Linux,
                LegacyArtifactInputs {
                    adp_binary_path: Some(Path::new("other/adp")),
                    core_agent_binary_path: Some(Path::new("other/agent")),
                    ..Default::default()
                },
            ),
            (
                Os::Windows,
                LegacyArtifactInputs {
                    alpine_image: Some("other:alpine"),
                    ..Default::default()
                },
            ),
            (
                Os::MacOs,
                LegacyArtifactInputs {
                    image_overrides: &overrides,
                    ..Default::default()
                },
            ),
        ] {
            check_inputs(&plans(INTEGRATION_CASE, os, true), os, &inputs).unwrap();
        }
    }

    #[test]
    fn image_override_names_are_still_validated_under_run_artifacts() {
        let plans = plans(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux, true);
        for entries in [
            vec!["baseline=x", "baseline=y"],
            vec!["unknown=x"],
            vec!["unknown=x", "unknown=y"],
            vec!["baseline=x", "unknown=y", "baseline=z"],
            vec!["baseline=x", "baseline=y", "unknown=z"],
        ] {
            let overrides: Vec<_> = entries.into_iter().map(|entry| entry.parse().unwrap()).collect();
            let expected = build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&overrides))
                .err()
                .unwrap()
                .to_string();
            let inputs = LegacyArtifactInputs {
                image_overrides: &overrides,
                alpine_image: Some("other:alpine"),
                ..Default::default()
            };
            assert_eq!(
                check_inputs(&plans, Os::Linux, &inputs).unwrap_err().to_string(),
                expected
            );
        }
        let macos = self::plans(INTEGRATION_CASE, Os::MacOs, true);
        let overrides = ["container=x".parse().unwrap()];
        let inputs = LegacyArtifactInputs {
            image_overrides: &overrides,
            ..Default::default()
        };
        assert_eq!(
            check_inputs(&macos, Os::MacOs, &inputs).unwrap_err().to_string(),
            "No test in this run uses a container named 'container'. Available: intake."
        );
    }

    #[test]
    fn an_override_that_passes_name_validation_but_has_no_role_is_a_disagreement() {
        let plans = plans(INTEGRATION_CASE, Os::Linux, true);
        let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
        let built = build_runners(&plans, Os::Linux, &artifacts).unwrap();
        let RunnerArtifacts::Bound(bound) = artifacts else {
            unreachable!()
        };
        let overrides = ["container=x".parse().unwrap()];
        let inputs = LegacyArtifactInputs {
            image_overrides: &overrides,
            ..Default::default()
        };
        assert_eq!(bound.check_legacy_inputs(&inputs, Os::MacOs, &[built[0].runner().unwrap()]).unwrap_err().to_string(),
            "Legacy artifact inputs disagree with the --artifacts bindings:\n  - --image-override container (PANORAMIC_IMAGE_OVERRIDES) names no artifact role on macos"
        );
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
            ..Default::default()
        };
        let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
        let built = build_runners(&plans, Os::Linux, &artifacts).unwrap();
        let RunnerArtifacts::Bound(bound) = artifacts else {
            unreachable!()
        };
        bound
            .check_legacy_inputs(&inputs, Os::Linux, &[built[0].runner().unwrap()])
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
    fn bound_integration_settings_take_the_target_and_intake_images_from_their_roles_on_each_runtime() {
        for (os, target) in [
            (Os::Linux, Some("target:linux")),
            (Os::Windows, Some("target:windows")),
            (Os::MacOs, None),
        ] {
            let yaml = format!("type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, windows, mac]\nprocedure: []\nintake: {{enabled: {}}}\n", os == Os::Linux);
            let plans = plans(&yaml, os, true);
            let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
            let settings = artifacts.integration_settings(runtime_for_os(os));
            assert_eq!(settings.runtime, runtime_for_os(os));
            assert_eq!(settings.target_image.as_deref(), target);
            assert_eq!(
                settings.intake_image,
                if os == Os::Linux {
                    "tools:bound"
                } else {
                    TOOLS_LINUX_IMAGE.local_default
                }
            );
            let CaseSchema::Typed(TypedConfig::Integration(config)) = plans[0].case().schema() else {
                panic!("expected integration")
            };
            let case = IntegrationTestCase::new(config.clone(), &settings);
            assert_eq!(case.image(TARGET_IMAGE_NAME), target);
            assert_eq!(
                case.image(INTAKE_IMAGE_NAME),
                (os == Os::Linux).then_some("tools:bound")
            );
        }
    }

    #[test]
    fn bound_correctness_runners_replace_every_file_image_with_role_references() {
        for manifest in [false, true] {
            for runtime in ["docker", "kubernetes_in_docker"] {
                let yaml = format!("type: correctness\n{}datadog_intake: {{image: 'intake:file'}}\nmillstone: {{image: 'millstone:file'}}\n",
                    CORRECTNESS_FIELDS.replace("runtime: docker", &format!("runtime: {runtime}")));
                let plans = plans(&yaml, Os::Linux, manifest);
                let artifacts = RunnerArtifacts::Bound(BoundArtifacts::new(needed_artifacts(&plans)));
                let runners = build_runners(&plans, Os::Linux, &artifacts).unwrap();
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
    fn bound_runner_settings_take_host_binaries_the_install_main_binary_and_the_fixup_image() {
        let yaml = "type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, mac]\nprocedure: []\n";
        let mut plans = plans(yaml, Os::MacOs, true);
        plans.extend(self::plans(yaml, Os::Linux, true));
        let mut settings = default_settings();
        let mounts = settings.mounts_dir.clone();
        BoundArtifacts::new(needed_artifacts(&plans)).apply_runner_settings(&mut settings);
        assert_eq!(settings.adp_binary_path, Path::new("build/adp"));
        assert_eq!(
            settings.core_agent_binary_path,
            Path::new("build/install/bin/agent/agent")
        );
        assert_eq!(settings.alpine_image, "fixup:bound");
        assert_eq!(settings.mounts_dir, mounts);
    }

    #[test]
    fn settings_whose_roles_no_plan_needs_keep_their_defaults() {
        let plans = plans(
            "type: integration\nname: case\ntimeout: 1s\nruntimes: [windows]\nprocedure: []\n",
            Os::Windows,
            true,
        );
        let bound = BoundArtifacts::new(needed_artifacts(&plans));
        let mut settings = default_settings();
        let defaults = settings.clone();
        bound.apply_runner_settings(&mut settings);
        assert_eq!(settings.adp_binary_path, defaults.adp_binary_path);
        assert_eq!(settings.core_agent_binary_path, defaults.core_agent_binary_path);
        assert_eq!(settings.alpine_image, defaults.alpine_image);
        let integration = RunnerArtifacts::Bound(bound).integration_settings(LINUX_RUNTIME);
        assert_eq!(
            integration.target_image.as_deref(),
            Some(AgentAdp::LINUX_IMAGE.local_default)
        );
        assert_eq!(integration.intake_image, TOOLS_LINUX_IMAGE.local_default);
    }

    #[test]
    fn override_runner_artifacts_build_the_same_runners_as_before() {
        let plans = plans(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux, true);
        let entries = [
            "baseline=base:override".parse().unwrap(),
            "millstone=millstone:override".parse().unwrap(),
        ];
        let runners = build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&entries)).unwrap();
        assert_eq!(
            runners[0].runner().unwrap().images(),
            BTreeMap::from([
                ("baseline", "base:override".into()),
                ("comparison", "comp:file".into()),
                ("datadog-intake", TOOLS_LINUX_IMAGE.local_default.into()),
                ("millstone", "millstone:override".into()),
            ])
        );
        let defaults = build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&[])).unwrap();
        assert_eq!(defaults[0].runner().unwrap().images()["baseline"], "base:file");
        for entries in [
            vec!["unused=x".parse().unwrap()],
            vec!["baseline=x".parse().unwrap(), "baseline=y".parse().unwrap()],
        ] {
            assert!(build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&entries)).is_err());
        }
    }
}
