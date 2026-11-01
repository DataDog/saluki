//! Maps artifact choices onto the integration and correctness runners' settings.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use panoramic_core::{
    artifact::{ArtifactBinding, ArtifactKind, ArtifactRole},
    backend::FIXUP_IMAGE,
};
use panoramic_participants::{AgentAdp, TOOLS_LINUX_IMAGE};

use super::{INTAKE_IMAGE_NAME, LINUX_RUNTIME, TARGET_IMAGE_NAME, WINDOWS_RUNTIME};
use crate::{
    correctness::config::Config as CorrectnessConfig,
    image_override::{ImageOverride, ImageOverrides},
    integration::IntegrationSettings,
    test::RunnerSettings,
};

/// Where the integration and correctness runners take their images and binaries from.
pub(crate) enum RunnerArtifacts<'a> {
    /// Case files and harness defaults, replaced by image overrides.
    Overrides(&'a [ImageOverride]),
    /// References bound to artifact roles.
    Bound(BoundArtifacts),
}

/// Needed artifact references, keyed by role name.
pub(crate) struct BoundArtifacts {
    references: BTreeMap<&'static str, String>,
}

impl BoundArtifacts {
    /// Indexes bindings collected from every case in scope.
    pub(crate) fn new(bindings: Vec<ArtifactBinding>) -> Self {
        Self {
            references: bindings
                .into_iter()
                .map(|binding| (binding.role.name, binding.reference))
                .collect(),
        }
    }

    fn reference(&self, role: ArtifactRole) -> Option<&str> {
        self.references.get(role.name).map(String::as_str)
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
                    runners[0].runner.images(),
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
            runners[0].runner.images(),
            BTreeMap::from([
                ("baseline", "base:override".into()),
                ("comparison", "comp:file".into()),
                ("datadog-intake", TOOLS_LINUX_IMAGE.local_default.into()),
                ("millstone", "millstone:override".into()),
            ])
        );
        let defaults = build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&[])).unwrap();
        assert_eq!(defaults[0].runner.images()["baseline"], "base:file");
        for entries in [
            vec!["unused=x".parse().unwrap()],
            vec!["baseline=x".parse().unwrap(), "baseline=y".parse().unwrap()],
        ] {
            assert!(build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&entries)).is_err());
        }
    }
}
