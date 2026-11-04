//! Case-supplied commands, files and Unix-stream listeners in Linux containers.

use std::collections::BTreeMap;

use panoramic_core::{
    artifact::ArtifactRole,
    endpoint::{Endpoint, Evidence},
    layout::{Os, Realization, RealizationKind},
    participant::{
        CaseValues, EffectiveConfig, ExitChannel, FactDeclaration, OutputChannel, ParticipantType, ProcessSpec,
        RenderedConfig,
    },
    recipe::{ContainerRecipe, Recipe},
};
use serde_json::Value;

use crate::{CASE_FILES_DIR, TOOLS_LINUX_IMAGE};

/// A case-supplied command running in the tools image.
pub struct Fixture;

impl Fixture {
    /// Participant type name used in case files.
    pub(crate) const NAME: &'static str = "fixture";

    fn command(params: &BTreeMap<String, Value>) -> Result<Vec<&str>, String> {
        let invalid = || "fixture params.command must be a nonempty array of strings".to_string();
        params
            .get("command")
            .and_then(Value::as_array)
            .filter(|command| !command.is_empty())
            .ok_or_else(invalid)?
            .iter()
            .map(|arg| arg.as_str().ok_or_else(invalid))
            .collect()
    }

    fn socket_endpoints(params: &BTreeMap<String, Value>) -> Result<BTreeMap<String, Endpoint>, String> {
        let Some(endpoints) = params.get("endpoints") else {
            return Ok(BTreeMap::new());
        };
        let endpoints = endpoints.as_object().ok_or_else(|| {
            "fixture params.endpoints must be a map of endpoint names to {unix: <absolute path>}".to_string()
        })?;
        endpoints
            .iter()
            .map(|(name, endpoint)| {
                let path = endpoint
                    .as_object()
                    .filter(|endpoint| endpoint.len() == 1)
                    .and_then(|endpoint| endpoint.get("unix"))
                    .and_then(Value::as_str)
                    .filter(|path| path.starts_with('/'))
                    .ok_or_else(|| {
                        format!("fixture params.endpoints.{name} must be exactly {{unix: <absolute path>}}")
                    })?;
                Ok((
                    name.clone(),
                    Endpoint::unix_stream(path).with_ready(Evidence::socket_exists(name)),
                ))
            })
            .collect()
    }
}

impl ParticipantType for Fixture {
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
        let mut recipe = ContainerRecipe::new(TOOLS_LINUX_IMAGE)
            .with_entrypoint(Self::command(config.params())?)
            .with_env(config.env().clone());
        for (name, contents) in config.files() {
            let relative = name
                .strip_prefix("files/")
                .ok_or_else(|| format!("internal error: fixture received undeclared configuration file '{name}'"))?;
            recipe = recipe.with_file(format!("{CASE_FILES_DIR}/{relative}"), 0o644, contents.clone());
        }
        Ok(Recipe::Container(recipe))
    }

    fn config_files(&self) -> &'static [&'static str] {
        &["files/"]
    }

    fn params(&self) -> &'static [&'static str] {
        &["command", "endpoints"]
    }

    fn validate_config(&self, _: Realization, config: &CaseValues) -> Result<(), String> {
        Self::command(&config.params)?;
        Self::socket_endpoints(&config.params)?;
        Ok(())
    }

    fn endpoints(&self, _: Realization, config: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        Self::socket_endpoints(&config.params)
    }

    fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
        if self.supports().contains(&(realization.os, realization.kind)) {
            &[FactDeclaration {
                name: "files_dir",
                value: CASE_FILES_DIR,
            }]
        } else {
            &[]
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["fixture"]
    }

    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        &[ProcessSpec {
            name: "fixture",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }]
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        endpoint::{test_support::assert_socket_endpoint, EndpointKind},
        recipe::test_support::{assert_container, files, rendered_with_params},
    };
    use serde_json::json;

    use super::*;

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    #[test]
    fn fixture_requires_a_nonempty_string_command() {
        for command in [
            None,
            Some(Value::Null),
            Some(json!([])),
            Some(json!("python3")),
            Some(json!({})),
            Some(json!(["python3", 1])),
        ] {
            let config = CaseValues {
                params: command
                    .map(|value| BTreeMap::from([("command".into(), value)]))
                    .unwrap_or_default(),
                ..Default::default()
            };
            assert_eq!(
                Fixture.validate_config(LINUX, &config).unwrap_err(),
                "fixture params.command must be a nonempty array of strings"
            );
        }
        let config = CaseValues {
            params: BTreeMap::from([(
                "command".into(),
                json!(["python3", "script.py", "{{ server.late.container_id }}", ""]),
            )]),
            ..Default::default()
        };
        Fixture.validate_config(LINUX, &config).unwrap();
    }

    #[test]
    fn fixture_endpoints_must_be_absolute_unix_paths_with_no_other_keys() {
        for (endpoints, field) in [
            (Value::Null, "params.endpoints"),
            (json!("/socket"), "params.endpoints"),
            (json!([]), "params.endpoints"),
            (json!({"api": null}), "params.endpoints.api"),
            (json!({"api": "/socket"}), "params.endpoints.api"),
            (json!({"api": []}), "params.endpoints.api"),
            (json!({"api": {}}), "params.endpoints.api"),
            (json!({"api": {"unix": null}}), "params.endpoints.api"),
            (json!({"api": {"unix": 123}}), "params.endpoints.api"),
            (json!({"api": {"unix": {}}}), "params.endpoints.api"),
            (json!({"api": {"unix": []}}), "params.endpoints.api"),
            (json!({"api": {"unix": ""}}), "params.endpoints.api"),
            (json!({"api": {"unix": "relative/socket"}}), "params.endpoints.api"),
            (json!({"api": {"tcp": 1234}}), "params.endpoints.api"),
            (
                json!({"api": {"unix": "/socket", "extra": true}}),
                "params.endpoints.api",
            ),
        ] {
            let config = CaseValues {
                params: BTreeMap::from([
                    ("command".into(), json!(["python3"])),
                    ("endpoints".into(), endpoints.clone()),
                ]),
                ..Default::default()
            };
            let error = Fixture.validate_config(LINUX, &config).unwrap_err();
            assert!(error.contains(field), "{endpoints}: {error}");
            assert_eq!(
                Fixture
                    .endpoints(
                        LINUX,
                        &EffectiveConfig {
                            params: config.params,
                            ..Default::default()
                        }
                    )
                    .unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn fixture_serves_each_declared_socket_with_socket_readiness() {
        let config = EffectiveConfig {
            params: BTreeMap::from([(
                "endpoints".into(),
                json!({
                    "api": {"unix": "/airlock/server/shared/api.sock"},
                    "events": {"unix": "/airlock/server/shared/events.sock"}
                }),
            )]),
            ..Default::default()
        };
        let endpoints = Fixture.endpoints(LINUX, &config).unwrap();
        assert_eq!(
            endpoints.keys().map(String::as_str).collect::<Vec<_>>(),
            ["api", "events"]
        );
        for name in ["api", "events"] {
            assert_socket_endpoint(
                &endpoints[name],
                EndpointKind::UnixStream,
                &format!("/airlock/server/shared/{name}.sock"),
                Some(Evidence::socket_exists(name)),
            );
        }
        for params in [BTreeMap::new(), BTreeMap::from([("endpoints".into(), json!({}))])] {
            assert!(Fixture
                .endpoints(
                    LINUX,
                    &EffectiveConfig {
                        params,
                        ..Default::default()
                    }
                )
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn fixture_recipe_runs_the_rendered_command_and_places_its_files() {
        let command = [
            "python3",
            "/var/lib/panoramic/files/serve.py",
            "container-id",
            "an argument with spaces",
            "",
            "$HOME",
        ];
        let params = BTreeMap::from([("command".into(), json!(command))]);
        let env = BTreeMap::from([("SETTING".into(), "rendered value".into())]);
        let config = rendered_with_params(env.clone(), params.clone(), BTreeMap::new());
        let recipe = Fixture.recipe(LINUX, &config).unwrap();
        assert_container(&recipe, TOOLS_LINUX_IMAGE, &command, &env);

        let config = rendered_with_params(
            env,
            params,
            BTreeMap::from([
                ("files/serve.py".into(), b"print('ready')".to_vec()),
                ("files/nested/data.bin".into(), vec![0, 255]),
            ]),
        );
        let recipe = Fixture.recipe(LINUX, &config).unwrap();
        assert_eq!(
            files(&recipe),
            [
                ("/var/lib/panoramic/files/nested/data.bin", 0o644, [0, 255].as_slice()),
                ("/var/lib/panoramic/files/serve.py", 0o644, b"print('ready')".as_slice()),
            ]
        );
    }

    #[test]
    fn fixture_recipe_rejects_an_undeclared_file() {
        let config = rendered_with_params(
            BTreeMap::new(),
            BTreeMap::from([("command".into(), json!(["python3"]))]),
            BTreeMap::from([("settings".into(), vec![])]),
        );
        assert_eq!(
            Fixture.recipe(LINUX, &config).err().unwrap(),
            "internal error: fixture received undeclared configuration file 'settings'"
        );
    }

    #[test]
    fn fixture_supports_only_linux_containers() {
        assert_eq!(Fixture.supports(), &[(Os::Linux, RealizationKind::LinuxContainer)]);
        assert_eq!(Fixture.artifacts(LINUX), &[TOOLS_LINUX_IMAGE]);
        assert_eq!(Fixture.config_files(), &["files/"]);
        assert_eq!(Fixture.params(), &["command", "endpoints"]);
        assert_eq!(Fixture.processes(), &["fixture"]);
        assert_eq!(
            Fixture.process_specs(LINUX),
            &[ProcessSpec {
                name: "fixture",
                exit: ExitChannel::Container,
                output: OutputChannel::Container
            }]
        );
        assert_eq!(
            Fixture
                .facts(LINUX)
                .iter()
                .map(|fact| (fact.name, fact.value))
                .collect::<Vec<_>>(),
            [("files_dir", "/var/lib/panoramic/files")]
        );
        assert!(Fixture.consumes(LINUX, &EffectiveConfig::default()).is_empty());
        assert!(Fixture.readiness(LINUX, &EffectiveConfig::default()).is_empty());
        assert!(Fixture.operations(LINUX).is_empty());
    }
}
