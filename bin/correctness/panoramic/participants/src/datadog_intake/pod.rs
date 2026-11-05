//! The mock intake in a single pod container.

use std::collections::BTreeMap;

use panoramic_core::recipe::{ContainerRecipe, PodRecipe, Recipe};

use crate::tools::TOOLS_LINUX_IMAGE;

pub(super) fn recipe(env: BTreeMap<String, String>) -> Recipe {
    Recipe::Pod(PodRecipe::new(
        "datadog-intake",
        ContainerRecipe::new(TOOLS_LINUX_IMAGE)
            .with_entrypoint(["/usr/local/bin/datadog-intake"])
            .with_command(Vec::<String>::new())
            .with_env(env),
    ))
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        layout::{Os, Realization, RealizationKind},
        participant::ParticipantType,
        recipe::test_support::{assert_pod, files, rendered},
    };

    use super::*;
    use crate::DatadogIntake;

    #[test]
    fn pod_runs_the_linux_intake_with_an_empty_command_and_the_case_environment() {
        let env = BTreeMap::from([
            ("INTAKE_HTTP_LISTEN_ADDR".into(), "0.0.0.0:8080".into()),
            ("INTAKE_DOGSTATSD_LISTEN_ADDR".into(), "[::]:8125".into()),
        ]);
        let recipe = DatadogIntake
            .recipe(
                Realization {
                    os: Os::Linux,
                    kind: RealizationKind::KubernetesPod,
                },
                &rendered(env.clone(), BTreeMap::new()),
            )
            .unwrap();
        assert_pod(
            &recipe,
            "datadog-intake",
            TOOLS_LINUX_IMAGE,
            &["/usr/local/bin/datadog-intake"],
            &[],
            &env,
        );
        assert!(files(&recipe).is_empty());
    }
}
