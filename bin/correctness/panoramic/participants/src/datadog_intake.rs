//! The mock intake's listeners and Linux container recipe.

use std::{collections::BTreeMap, net::SocketAddr};

use panoramic_core::{
    artifact::ArtifactRole,
    endpoint::{Endpoint, Evidence, Port},
    layout::{Os, Realization, RealizationKind},
    participant::{
        CaseValues, EffectiveConfig, ExitChannel, OutputChannel, ParticipantType, ProcessSpec, RenderedConfig,
    },
    recipe::{ContainerRecipe, Recipe},
};

use crate::TOOLS_LINUX_IMAGE;

/// The mock Datadog intake, with independent telemetry storage per participant.
pub struct DatadogIntake;

impl DatadogIntake {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "datadog-intake";
}

impl ParticipantType for DatadogIntake {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }

    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[TOOLS_LINUX_IMAGE]
    }

    fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        Ok(Recipe::Container(
            ContainerRecipe::new(TOOLS_LINUX_IMAGE)
                .with_entrypoint(["/usr/local/bin/datadog-intake"])
                .with_env(config.env().clone()),
        ))
    }

    fn validate_config(&self, _: Realization, config: &CaseValues) -> Result<(), String> {
        listen_ports(&config.env).map(|_| ())
    }

    fn endpoints(&self, _: Realization, config: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        let [http, dogstatsd] = listen_ports(&config.env)?;
        // The intake binds UDP before HTTP, so /ready covers both listeners.
        let ready = Evidence::http_ok("api", "/ready");
        Ok(BTreeMap::from([
            (
                "api".into(),
                Endpoint::http(Port::Fixed(http)).with_ready(ready.clone()),
            ),
            (
                "dogstatsd_forward".into(),
                Endpoint::udp(Port::Fixed(dogstatsd)).with_ready(ready),
            ),
        ]))
    }

    fn processes(&self) -> &'static [&'static str] {
        &["intake"]
    }

    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        &[ProcessSpec {
            name: "intake",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }]
    }
}

fn listen_ports(env: &BTreeMap<String, String>) -> Result<[u16; 2], String> {
    let mut ports = [0; 2];
    for (index, (key, default)) in [
        ("INTAKE_HTTP_LISTEN_ADDR", "0.0.0.0:2049"),
        ("INTAKE_DOGSTATSD_LISTEN_ADDR", "0.0.0.0:9125"),
    ]
    .into_iter()
    .enumerate()
    {
        let value = env.get(key).map_or(default, String::as_str);
        let address: SocketAddr = value
            .parse()
            .map_err(|error| format!("{key}='{value}' must be a listen address in host:port form: {error}"))?;
        if address.port() == 0 {
            return Err(format!("{key}='{value}' must use a nonzero port"));
        }
        if !address.ip().is_unspecified() {
            return Err(format!(
                "{key}='{value}' must bind 0.0.0.0 or [::] so other participants and published ports can reach it"
            ));
        }
        ports[index] = address.port();
    }
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        endpoint::{test_support::assert_network_endpoint, EndpointKind},
        recipe::test_support::{assert_container, rendered_env},
    };

    use super::*;

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    #[test]
    fn the_intake_listens_on_historical_ports_unless_its_environment_overrides_them() {
        for (env, ports) in [
            (BTreeMap::new(), [2049, 9125]),
            (
                BTreeMap::from([
                    ("INTAKE_HTTP_LISTEN_ADDR".into(), "0.0.0.0:8080".into()),
                    ("INTAKE_DOGSTATSD_LISTEN_ADDR".into(), "[::]:8125".into()),
                ]),
                [8080, 8125],
            ),
        ] {
            DatadogIntake
                .validate_config(
                    LINUX,
                    &CaseValues {
                        env: env.clone(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let endpoints = DatadogIntake
                .endpoints(
                    LINUX,
                    &EffectiveConfig {
                        env,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(endpoints.len(), 2);
            for (name, kind, port) in [
                ("api", EndpointKind::Http, ports[0]),
                ("dogstatsd_forward", EndpointKind::Udp, ports[1]),
            ] {
                assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::http_ok("api", "/ready")));
            }
        }
    }

    #[test]
    fn an_intake_listen_address_must_have_a_nonzero_port_and_an_unspecified_host() {
        for key in ["INTAKE_HTTP_LISTEN_ADDR", "INTAKE_DOGSTATSD_LISTEN_ADDR"] {
            for (value, expected) in [
                ("0.0.0.0", "host:port"),
                ("0.0.0.0:invalid", "host:port"),
                ("0.0.0.0:65536", "host:port"),
                ("0.0.0.0:0", "nonzero port"),
                ("127.0.0.1:2049", "must bind 0.0.0.0 or [::]"),
                ("[::1]:2049", "must bind 0.0.0.0 or [::]"),
            ] {
                let env = BTreeMap::from([(key.into(), value.into())]);
                let error = DatadogIntake
                    .validate_config(
                        LINUX,
                        &CaseValues {
                            env: env.clone(),
                            ..Default::default()
                        },
                    )
                    .unwrap_err();
                assert!(
                    error.contains(key) && error.contains(value) && error.contains(expected),
                    "{error}"
                );
                assert_eq!(
                    DatadogIntake
                        .endpoints(
                            LINUX,
                            &EffectiveConfig {
                                env,
                                ..Default::default()
                            }
                        )
                        .unwrap_err(),
                    error
                );
            }
        }
    }

    #[test]
    fn the_intake_runs_one_process_observed_through_its_container() {
        assert_eq!(DatadogIntake.processes(), &["intake"]);
        assert_eq!(
            DatadogIntake.process_specs(LINUX),
            &[ProcessSpec {
                name: "intake",
                exit: ExitChannel::Container,
                output: OutputChannel::Container,
            }]
        );
    }

    #[test]
    fn the_intake_recipe_runs_the_tools_binary_with_the_case_environment() {
        let env = BTreeMap::from([("INTAKE_HTTP_LISTEN_ADDR".into(), "0.0.0.0:8080".into())]);
        let config = rendered_env(env.clone());
        let recipe = DatadogIntake.recipe(LINUX, &config).unwrap();
        assert_container(&recipe, TOOLS_LINUX_IMAGE, &["/usr/local/bin/datadog-intake"], &env);
    }

    #[test]
    fn the_intake_supports_only_linux_containers_and_declares_only_the_tools_image() {
        assert_eq!(
            DatadogIntake.supports(),
            &[(Os::Linux, RealizationKind::LinuxContainer)]
        );
        assert_eq!(DatadogIntake.artifacts(LINUX), &[TOOLS_LINUX_IMAGE]);
        assert!(DatadogIntake.facts(LINUX).is_empty());
        assert!(DatadogIntake.consumes(LINUX, &EffectiveConfig::default()).is_empty());
        assert!(DatadogIntake.readiness(LINUX, &EffectiveConfig::default()).is_empty());
        assert!(DatadogIntake.config_files().is_empty());
        assert!(DatadogIntake.params().is_empty());
    }
}
