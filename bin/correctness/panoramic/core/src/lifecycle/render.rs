//! Renders creation-time references and case files for participant recipes.

use std::collections::BTreeMap;

use crate::{
    backend::LateFact,
    case::{
        config_references::{walk_config_strings_mut, ConfigField},
        template::{Reference, Template},
    },
    participant::RenderedConfig,
    plan::CasePlan,
};

pub(super) fn render<C>(
    plan: &CasePlan<C>, participant: &str, created: &BTreeMap<String, String>,
) -> Result<RenderedConfig, String> {
    let config = plan
        .effective_config(participant)
        .ok_or_else(|| format!("internal error: participant '{participant}' has no planned configuration"))?;
    let substitute = |template: &Template, field: ConfigField<'_>| {
        template
            .substitute(|written, reference| {
                let value = match reference {
                    Reference::Fact {
                        participant: target,
                        fact,
                    } => plan.fact(target.name(participant), fact).map(str::to_owned),
                    Reference::Late {
                        participant: target, ..
                    } if target.name(participant) == participant => return Ok(None),
                    Reference::Late {
                        participant: target,
                        fact: LateFact::ContainerId,
                    } => created.get(target.name(participant)).cloned(),
                    Reference::Endpoint {
                        participant: target,
                        endpoint,
                        part,
                    } => plan
                        .endpoint_address(participant, target.name(participant), endpoint)
                        .and_then(|address| address.part(*part)),
                    Reference::Connection { consumed, part } => plan
                        .connection_address(participant, consumed)
                        .and_then(|address| address.part(*part)),
                    Reference::Late { .. } => None,
                };
                value
                    .map(Some)
                    .ok_or_else(|| format!("internal error: no creation-time value for '{written}'"))
            })
            .map_err(|error| format!("participant '{participant}' {field}: {error}"))
    };
    let mut rendered = RenderedConfig {
        env: config.env.clone(),
        params: config.params.clone(),
        files: BTreeMap::new(),
    };
    walk_config_strings_mut(&mut rendered.env, &mut rendered.params, |field, text| {
        let template = text
            .parse()
            .map_err(|error| format!("participant '{participant}' {field}: {error}"))?;
        *text = substitute(&template, field)?;
        Ok(())
    })?;
    for (name, source) in &config.files {
        let bytes = if source.template {
            substitute(plan.case().file_templates().get(&source.from), ConfigField::File(name))?.into_bytes()
        } else {
            let path = plan
                .case()
                .source()
                .parent()
                .expect("case sources have a directory")
                .join(&source.from);
            let field = format!("participant '{participant}' file '{name}' ({})", path.display());
            let metadata = std::fs::metadata(&path).map_err(|error| format!("{field}: {error}"))?;
            if metadata.is_dir() {
                return Err(format!("{field} is a directory; native creation places regular files"));
            }
            if !metadata.is_file() {
                return Err(format!("{field} is not a regular file"));
            }
            std::fs::read(&path).map_err(|error| format!("{field}: {error}"))?
        };
        rendered.files.insert(name.clone(), bytes);
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::{
        artifact::ArtifactRole,
        case::{
            fixtures::{minimal_procedure, write_case, Client, Configured, FixtureConfig, FixtureTypes, Server, STEPS},
            CaseLoader, Registries,
        },
        endpoint::{ConsumedEndpoint, Endpoint},
        layout::{Os, Realization, RealizationKind},
        participant::{EffectiveConfig, ParticipantType},
        plan::Planner,
    };

    struct FileClient;

    impl ParticipantType for FileClient {
        fn name(&self) -> &'static str {
            "file-client"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            Server.supports()
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn config_files(&self) -> &'static [&'static str] {
            Configured.config_files()
        }

        fn params(&self) -> &'static [&'static str] {
            Configured.params()
        }

        fn endpoints(
            &self, _realization: Realization, _config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            Ok(BTreeMap::from([(
                "socket".into(),
                Endpoint::unix_stream("/run/input.sock"),
            )]))
        }

        fn consumes(&self, realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            Client.consumes(realization, config)
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    fn compile(config: &str, source: &Path) -> CasePlan<FixtureConfig> {
        let registries = Registries {
            participant_types: &[&FileClient, &Server],
            steps: STEPS,
            ..Default::default()
        };
        let yaml = format!(
            "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-client, subject: true, connect: {{upstream: server.api}}, config: {config}}}\n  server: {{type: server, config: {{env: {{OWN: '{{{{  self.late.ip  }}}}/{{{{server.late.ip}}}}'}}}}}}\n{}\n",
            minimal_procedure!()
        );
        let case = CaseLoader::new(registries, FixtureTypes, "fixture")
            .load_content(&yaml, "alpha", source)
            .unwrap()
            .remove(0);
        Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[test]
    fn rendering_substitutes_created_container_ids_and_planned_addresses_in_env_params_and_template_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let text = concat!(
            "{{server.late.container_id}}|{{server.facts.file}}|{{server.endpoints.api.url}}|",
            "{{server.endpoints.api.host}}|{{server.endpoints.api.port}}|{{self.connections.upstream.url}}|",
            "{{self.connections.upstream.host}}|{{self.connections.upstream.port}}|",
            "{{self.endpoints.socket.path}}|{{self.endpoints.socket.url}}"
        );
        let template_path = source.parent().unwrap().join("input.txt");
        std::fs::write(&template_path, text).unwrap();
        let config = format!(
            "{{env: {{VALUE: '{text}'}}, params: {{mode: [{{nested: ['{text}', false, 7, null]}}]}}, files: {{settings.yaml: {{from: input.txt, template: true}}}}}}"
        );
        let plan = compile(&config, &source);
        std::fs::remove_file(template_path).unwrap();
        let created = BTreeMap::from([("server".into(), "created-id".into())]);
        let rendered = render(&plan, "subject", &created).unwrap();
        let expected = concat!(
            "created-id|/logs/output.log|http://server:8080|server|8080|",
            "http://server:8080|server|8080|/run/input.sock|unix:///run/input.sock"
        );
        assert_eq!(rendered.env["VALUE"], expected);
        assert_eq!(rendered.env["UPSTREAM_URL"], "http://server:8080");
        assert_eq!(
            rendered.params["mode"],
            json!([{ "nested": [expected, false, 7, null] }])
        );
        assert_eq!(rendered.files["settings.yaml"], expected.as_bytes());
        for (config, field) in [
            (
                "{env: {ID: '{{server.late.container_id}}'}}",
                "environment variable 'ID'",
            ),
            (
                "{params: {mode: [{nested: '{{server.late.container_id}}'}]}}",
                "parameter 'mode'",
            ),
            (
                "{files: {settings.yaml: {from: input.txt, template: true}}}",
                "configuration file 'settings.yaml'",
            ),
        ] {
            std::fs::write(
                source.parent().unwrap().join("input.txt"),
                "{{server.late.container_id}}",
            )
            .unwrap();
            let plan = compile(config, &source);
            assert_eq!(
                render(&plan, "subject", &BTreeMap::new()).err().unwrap(),
                format!("participant 'subject' {field}: internal error: no creation-time value for '{{{{server.late.container_id}}}}'")
            );
        }
    }

    #[test]
    fn rendering_keeps_the_participants_own_late_facts_as_written_for_its_type() {
        let plan = compile("{}", Path::new("cases/alpha/config.yaml"));
        let rendered = render(&plan, "server", &BTreeMap::new()).unwrap();
        assert_eq!(rendered.env["OWN"], "{{  self.late.ip  }}/{{server.late.ip}}");
    }

    #[test]
    fn rendering_reads_regular_case_files_and_rejects_directory_sources() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let path = source.parent().unwrap().join("input.bin");
        let bytes = b"\xff\0{{not a reference}}";
        std::fs::write(&path, bytes).unwrap();
        let plan = compile("{files: {settings.yaml: input.bin}}", &source);
        let rendered = render(&plan, "subject", &BTreeMap::new()).unwrap();
        assert_eq!(rendered.files["settings.yaml"], bytes);
        std::fs::remove_file(&path).unwrap();
        let error = render(&plan, "subject", &BTreeMap::new()).err().unwrap();
        assert!(error.starts_with(&format!(
            "participant 'subject' file 'settings.yaml' ({}): ",
            path.display()
        )));
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            render(&plan, "subject", &BTreeMap::new()).err().unwrap(),
            format!(
                "participant 'subject' file 'settings.yaml' ({}) is a directory; native creation places regular files",
                path.display()
            )
        );
        #[cfg(unix)]
        {
            std::fs::remove_dir(&path).unwrap();
            let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let error = render(&plan, "subject", &BTreeMap::new()).err().unwrap();
            assert!(error.ends_with("is not a regular file"));
        }
    }
}
