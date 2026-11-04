//! Renders creation-time references and case files for participant recipes.

use std::{collections::BTreeMap, path::Path};

use crate::{
    backend::{shared_root, LateFact},
    case::{
        config_references::{walk_config_strings_mut, ConfigField},
        template::{Reference, Template},
    },
    participant::RenderedConfig,
    plan::{participant_dir, ParticipantPlan},
};

pub(super) fn render(
    plan: ParticipantPlan<'_>, participant: &str, created: &BTreeMap<String, String>,
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
    let realization = plan
        .layout()
        .get(participant)
        .expect("planned participant has a realization");
    let root = shared_root(realization.kind, realization.os, plan.scope());
    let mut rendered = RenderedConfig {
        directory: participant_dir(&root, participant, realization.os),
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
            substitute(plan.templates().get(&source.from), ConfigField::File(name))?.into_bytes()
        } else {
            let path = plan
                .source()
                .parent()
                .expect("case sources have a directory")
                .join(&source.from);
            let field = format!("participant '{participant}' file '{name}' ({})", path.display());
            let metadata = std::fs::metadata(&path).map_err(|error| format!("{field}: {error}"))?;
            if name.ends_with('/') {
                if !metadata.is_dir() {
                    return Err(format!("{field} is not a directory"));
                }
                render_directory(participant, name, &path, &mut rendered.files)?;
                continue;
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

fn render_directory(
    participant: &str, name: &str, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let mut pending = vec![(directory.to_path_buf(), name.to_owned())];
    while let Some((directory, prefix)) = pending.pop() {
        let field = format!("participant '{participant}' file '{name}' ({})", directory.display());
        let entries = std::fs::read_dir(&directory).map_err(|error| format!("{field}: {error}"))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{field}: {error}"))?;
            let path = entry.path();
            let field = format!("participant '{participant}' file '{name}' ({})", path.display());
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| format!("{field}: {error}"))?;
            if metadata.is_symlink() {
                return Err(format!(
                    "{field} is a symlink; directory sources cannot contain symlinks"
                ));
            }
            let file_name = entry.file_name();
            let file_name = file_name
                .to_str()
                .ok_or_else(|| format!("{field} has a non-UTF-8 name"))?;
            let key = format!("{prefix}{file_name}");
            if metadata.is_dir() {
                pending.push((path, format!("{key}/")));
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path).map_err(|error| format!("{field}: {error}"))?;
                files.insert(key, bytes);
            } else {
                return Err(format!("{field} is not a regular file or directory"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        plan::{CasePlan, Planner},
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
            &["files/", "settings.yaml"]
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
    fn rendered_configuration_names_the_participant_directory_under_each_realizations_shared_root() {
        let registries = Registries {
            participant_types: &[&Server],
            steps: STEPS,
            ..Default::default()
        };
        for (os, kind, directory) in [
            (Os::Linux, "linux-container", "/airlock/subject"),
            (Os::MacOs, "host-process-group", "/tmp/panoramic-testrun0/0000/subject"),
            (Os::Windows, "windows-container", r"C:\airlock\subject"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n  subject: {{type: server, subject: true, realization: {{kind: {kind}}}}}\n{}", os.name(), minimal_procedure!());
            let case = CaseLoader::new(registries, FixtureTypes, "fixture")
                .load_content(&yaml, "case", Path::new("cases/case/config.yaml"))
                .unwrap()
                .remove(0);
            let plan = Planner::with_run(registries, "testrun0")
                .compile(case, os)
                .unwrap()
                .into_plan()
                .unwrap();
            let rendered = render(plan.participants().unwrap(), "subject", &BTreeMap::new()).unwrap();
            assert_eq!(rendered.directory(), directory);
        }
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
        let rendered = render(plan.participants().unwrap(), "subject", &created).unwrap();
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
                render(plan.participants().unwrap(), "subject", &BTreeMap::new()).err().unwrap(),
                format!("participant 'subject' {field}: internal error: no creation-time value for '{{{{server.late.container_id}}}}'")
            );
        }
    }

    #[test]
    fn an_own_ip_reference_in_env_stays_canonical_through_rendering() {
        let plan = compile("{}", Path::new("cases/alpha/config.yaml"));
        let rendered = render(plan.participants().unwrap(), "server", &BTreeMap::new()).unwrap();
        assert_eq!(rendered.env["OWN"], "{{ self.late.ip }}/{{ self.late.ip }}");
    }

    #[test]
    fn a_directory_source_renders_one_entry_per_nested_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir_all(case_dir.join("input/z/nested")).unwrap();
        std::fs::create_dir_all(case_dir.join("input/a/empty")).unwrap();
        std::fs::write(case_dir.join("input/z/nested/data"), [0xff, 0, 0xfe]).unwrap();
        std::fs::write(case_dir.join("input/z/data"), "{{not a reference}}").unwrap();
        std::fs::write(case_dir.join("input/a/data"), "first").unwrap();
        std::fs::write(case_dir.join("input/empty"), "").unwrap();
        std::fs::write(case_dir.join("separate"), "settings").unwrap();
        let plan = compile("{files: {files/: input, settings.yaml: separate}}", &source);
        let rendered = render(plan.participants().unwrap(), "subject", &BTreeMap::new()).unwrap();
        assert_eq!(
            rendered
                .files()
                .iter()
                .map(|(key, bytes)| (key.as_str(), bytes.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                ("files/a/data", &b"first"[..]),
                ("files/empty", &b""[..]),
                ("files/z/data", &b"{{not a reference}}"[..]),
                ("files/z/nested/data", &b"\xff\0\xfe"[..]),
                ("settings.yaml", &b"settings"[..]),
            ],
        );
    }

    #[test]
    fn an_empty_directory_source_contributes_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        std::fs::create_dir_all(source.parent().unwrap().join("input/nested/empty")).unwrap();
        let plan = compile("{files: {files/: input}}", &source);
        let rendered = render(plan.participants().unwrap(), "subject", &BTreeMap::new()).unwrap();
        assert!(rendered.files().is_empty());
    }

    #[test]
    fn a_directory_overlay_replaces_the_whole_base_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir(case_dir.join("base")).unwrap();
        std::fs::create_dir(case_dir.join("overlay")).unwrap();
        std::fs::write(case_dir.join("base/only-in-base"), "base").unwrap();
        std::fs::write(case_dir.join("base/shared"), "base").unwrap();
        std::fs::write(case_dir.join("overlay/shared"), "overlay").unwrap();
        let plan = compile(
            "{files: {files/: base}, os: {linux: {files: {files/: overlay}}}}",
            &source,
        );
        let rendered = render(plan.participants().unwrap(), "subject", &BTreeMap::new()).unwrap();
        assert_eq!(
            rendered.files,
            BTreeMap::from([("files/shared".into(), b"overlay".to_vec())])
        );
    }

    #[test]
    fn rendering_rechecks_directory_sources_after_loading() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let path = source.parent().unwrap().join("input");
        std::fs::create_dir(&path).unwrap();
        let plan = compile("{files: {files/: input}}", &source);
        std::fs::remove_dir(&path).unwrap();
        let error = render(plan.participants().unwrap(), "subject", &BTreeMap::new())
            .err()
            .unwrap();
        assert!(error.starts_with(&format!("participant 'subject' file 'files/' ({}): ", path.display())));
        std::fs::write(&path, "").unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .err()
                .unwrap(),
            format!(
                "participant 'subject' file 'files/' ({}) is not a directory",
                path.display()
            ),
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_directory_source_fails_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir_all(case_dir.join("input/nested")).unwrap();
        std::fs::create_dir(case_dir.join("outside")).unwrap();
        std::fs::write(case_dir.join("outside/file"), "outside").unwrap();
        let link = case_dir.join("input/nested/link");
        for target in ["../../outside/file", "../../outside", ".", "../../missing"] {
            std::os::unix::fs::symlink(target, &link).unwrap();
            let plan = compile("{files: {files/: input}}", &source);
            assert_eq!(
                render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                    .err()
                    .unwrap(),
                format!(
                    "participant 'subject' file 'files/' ({}) is a symlink; directory sources cannot contain symlinks",
                    link.display(),
                ),
            );
            std::fs::remove_file(&link).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_inside_a_directory_source_fails_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let path = source.parent().unwrap().join("input/socket");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let plan = compile("{files: {files/: input}}", &source);
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .err()
                .unwrap(),
            format!(
                "participant 'subject' file 'files/' ({}) is not a regular file or directory",
                path.display()
            ),
        );
    }

    #[cfg(unix)]
    #[test]
    fn rendering_follows_top_level_symlinks_to_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir_all(dir.path().join("external/nested")).unwrap();
        std::fs::write(dir.path().join("external/nested/file"), "contents").unwrap();
        std::os::unix::fs::symlink("../external", case_dir.join("directory")).unwrap();
        std::os::unix::fs::symlink("../external/nested/file", case_dir.join("file")).unwrap();
        let plan = compile("{files: {files/: directory, settings.yaml: file}}", &source);
        let rendered = render(plan.participants().unwrap(), "subject", &BTreeMap::new()).unwrap();
        assert_eq!(
            rendered.files,
            BTreeMap::from([
                ("files/nested/file".into(), b"contents".to_vec()),
                ("settings.yaml".into(), b"contents".to_vec()),
            ]),
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_name_inside_a_directory_source_fails_rendering() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let directory = source.parent().unwrap().join("input");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(OsStr::from_bytes(b"\xff"));
        std::fs::write(&path, "").unwrap();
        let plan = compile("{files: {files/: input}}", &source);
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .err()
                .unwrap(),
            format!(
                "participant 'subject' file 'files/' ({}) has a non-UTF-8 name",
                path.display()
            ),
        );
    }

    #[test]
    fn rendering_reads_regular_case_files_and_rechecks_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let path = source.parent().unwrap().join("input.bin");
        let bytes = b"\xff\0{{not a reference}}";
        std::fs::write(&path, bytes).unwrap();
        let plan = compile("{files: {settings.yaml: input.bin}}", &source);
        let rendered = render(plan.participants().unwrap(), "subject", &BTreeMap::new()).unwrap();
        assert_eq!(rendered.files["settings.yaml"], bytes);
        std::fs::remove_file(&path).unwrap();
        let error = render(plan.participants().unwrap(), "subject", &BTreeMap::new())
            .err()
            .unwrap();
        assert!(error.starts_with(&format!(
            "participant 'subject' file 'settings.yaml' ({}): ",
            path.display()
        )));
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .err()
                .unwrap(),
            format!(
                "participant 'subject' file 'settings.yaml' ({}) is not a regular file",
                path.display()
            )
        );
        #[cfg(unix)]
        {
            std::fs::remove_dir(&path).unwrap();
            let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let error = render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .err()
                .unwrap();
            assert!(error.ends_with("is not a regular file"));
        }
    }
}
