//! Renders creation-time references and case files for participant recipes.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    backend::{shared_root, writes_late_files, LateFact},
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
    let realization = plan
        .layout()
        .get(participant)
        .expect("planned participant has a realization");
    let root = shared_root(realization.kind, realization.os, plan.scope());
    let mut rendered = RenderedConfig {
        directory: participant_dir(&root, participant, realization.os),
        env: config.env.clone(),
        params: config.params.clone(),
        files: config.files.clone(),
        late_files: BTreeSet::new(),
    };
    let mut substitute = |template: &Template, field: ConfigField<'_>| {
        template
            .substitute(|written, reference| {
                let own_late = matches!(reference, Reference::Late { participant: target, .. }
                    if target.name(participant) == participant);
                let value = match reference {
                    Reference::Fact {
                        participant: target,
                        fact,
                    } => plan.fact(target.name(participant), fact).map(str::to_owned),
                    Reference::Late { .. } if own_late => None,
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
                if value.is_none() && matches!(reference, Reference::Late { .. }) {
                    if let ConfigField::File(name) = field {
                        if writes_late_files(realization.kind) {
                            rendered.late_files.insert(name.to_owned());
                            return Ok(None);
                        }
                    }
                    if own_late {
                        return Ok(None);
                    }
                }
                value
                    .map(Some)
                    .ok_or_else(|| format!("internal error: no creation-time value for '{written}'"))
            })
            .map_err(|error| format!("participant '{participant}' {field}: {error}"))
    };
    walk_config_strings_mut(&mut rendered.env, &mut rendered.params, |field, text| {
        let template = text
            .parse()
            .map_err(|error| format!("participant '{participant}' {field}: {error}"))?;
        *text = substitute(&template, field)?;
        Ok(())
    })?;
    // Non-template entries, including expanded directories, keep the bytes snapshotted at planning.
    // Template entries are rendered again because creation-time values fill in late references.
    for (name, source) in plan
        .file_sources(participant)
        .iter()
        .filter(|(_, source)| source.template)
    {
        let bytes = substitute(plan.templates().get(&source.from), ConfigField::File(name))?.into_bytes();
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
            fixtures::{minimal_procedure, write_case, Configured, FixtureConfig, FixtureTypes, Server, STEPS},
            CaseLoader, Registries,
        },
        endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Port},
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
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::MacOs, RealizationKind::HostProcessGroup),
                (Os::Windows, RealizationKind::WindowsContainer),
                (Os::Linux, RealizationKind::KubernetesPod),
            ]
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn config_files(&self) -> &'static [&'static str] {
            &["files/", "raw.txt", "settings.yaml"]
        }

        fn params(&self) -> &'static [&'static str] {
            Configured.params()
        }

        fn endpoints(
            &self, _realization: Realization, _config: &EffectiveConfig,
        ) -> Result<BTreeMap<String, Endpoint>, String> {
            Ok(BTreeMap::from([
                ("socket".into(), Endpoint::unix_stream("/run/input.sock")),
                ("api".into(), Endpoint::http(Port::Fixed(8080))),
            ]))
        }

        fn consumes(&self, _realization: Realization, _config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            BTreeMap::from([(
                "upstream".into(),
                ConsumedEndpoint {
                    accepts: vec![EndpointKind::Http],
                    required: false,
                    delivery: Delivery::Env {
                        keys: BTreeSet::from(["UPSTREAM_URL".into()]),
                        value: |address| BTreeMap::from([("UPSTREAM_URL".into(), address.url.clone())]),
                    },
                },
            )])
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    fn try_compile(config: &str, source: &Path) -> Result<CasePlan<FixtureConfig>, String> {
        let yaml = format!(
            "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-client, subject: true, connect: {{upstream: server.api}}, config: {config}}}\n  server: {{type: server, config: {{env: {{OWN: '{{{{  self.late.ip  }}}}/{{{{server.late.ip}}}}'}}}}}}\n{}\n",
            minimal_procedure!()
        );
        try_compile_case(&yaml, source)
    }

    fn compile_case(yaml: &str, source: &Path) -> CasePlan<FixtureConfig> {
        try_compile_case(yaml, source).unwrap()
    }

    fn try_compile_case(yaml: &str, source: &Path) -> Result<CasePlan<FixtureConfig>, String> {
        let registries = Registries {
            participant_types: &[&FileClient, &Server],
            steps: STEPS,
            ..Default::default()
        };
        let case = CaseLoader::new(registries, FixtureTypes)
            .load_content("cases", yaml, "alpha", source)
            .map_err(|error| error.to_string())?
            .remove(0);
        Planner::with_run(registries, "testrun0")
            .compile(case, Os::Linux)
            .map_err(|error| error.to_string())
            .map(|compiled| compiled.into_plan().unwrap())
    }

    fn compile(config: &str, source: &Path) -> CasePlan<FixtureConfig> {
        try_compile(config, source).unwrap()
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
            let case = CaseLoader::new(registries, FixtureTypes)
                .load_content("cases", &yaml, "case", Path::new("cases/case/config.yaml"))
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
        let raw_path = source.parent().unwrap().join("raw.txt");
        std::fs::write(&raw_path, text).unwrap();
        let config = format!(
            "{{env: {{VALUE: '{text}'}}, params: {{mode: [{{nested: ['{text}', false, 7, null]}}]}}, files: {{settings.yaml: {{from: input.txt, template: true}}, raw.txt: raw.txt}}}}"
        );
        let plan = compile(&config, &source);
        std::fs::remove_file(template_path).unwrap();
        std::fs::remove_file(raw_path).unwrap();
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
        assert_eq!(rendered.files["raw.txt"], text.as_bytes());
        assert_eq!(
            plan.effective_config("subject").unwrap().files["raw.txt"],
            text.as_bytes()
        );
        assert_eq!(
            plan.effective_config("subject").unwrap().files["settings.yaml"],
            expected
                .replace("created-id", "{{server.late.container_id}}")
                .as_bytes()
        );
        assert!(rendered.late_files.is_empty());
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
    fn pod_template_files_defer_late_references_but_env_and_params_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let compile_pods = |config: &str| {
            let yaml = format!(
                "timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-client, subject: true, realization: {{kind: kubernetes-pod}}, connect: {{upstream: server.api}}, config: {config}}}\n  server: {{type: file-client}}\n  container: {{type: server, realization: {{kind: linux-container}}}}\n{}\n",
                minimal_procedure!()
            );
            compile_case(&yaml, &source)
        };
        let late = concat!(
            "{{self.late.pod_uid}}|{{ self.late.container_id }}|{{self.late.ip}}|",
            "{{subject.late.pod_uid}}|{{ subject.late.container_id }}|{{subject.late.ip}}|",
            "{{server.late.pod_uid}}|{{ server.late.container_id }}|{{server.late.ip}}|",
            "{{container.late.container_id}}|{{container.late.ip}}"
        );
        let text = format!(
            "{late}|{{{{container.facts.file}}}}|{{{{self.connections.upstream.url}}}}|{{{{self.connections.upstream.host}}}}|{{{{self.connections.upstream.port}}}}"
        );
        std::fs::write(source.parent().unwrap().join("input.txt"), text).unwrap();
        let plan = compile_pods("{files: {settings.yaml: {from: input.txt, template: true}}}");
        let created = BTreeMap::from([
            ("server".into(), "created-pod-id".into()),
            ("container".into(), "created-id".into()),
        ]);
        // Pod template files keep unavailable late references for the late write; env and params still need every
        // value at creation.
        for (created, expected) in [
            (&BTreeMap::new(), late.to_owned()),
            (
                &created,
                late.replace("{{ server.late.container_id }}", "created-pod-id")
                    .replace("{{container.late.container_id}}", "created-id"),
            ),
        ] {
            let rendered = render(plan.participants().unwrap(), "subject", created).unwrap();
            assert_eq!(
                rendered.files["settings.yaml"],
                format!("{expected}|/logs/output.log|http://server:8080|server|8080").as_bytes()
            );
            assert_eq!(rendered.env["UPSTREAM_URL"], "http://server:8080");
            assert_eq!(rendered.late_files, BTreeSet::from(["settings.yaml".into()]));
        }
        for (config, field) in [
            (
                "{env: {ID: '{{container.late.container_id}}'}}",
                "environment variable 'ID'",
            ),
            (
                "{params: {mode: [{nested: '{{container.late.container_id}}'}]}}",
                "parameter 'mode'",
            ),
        ] {
            let plan = compile_pods(config);
            let rendered = render(plan.participants().unwrap(), "subject", &created).unwrap();
            if field == "environment variable 'ID'" {
                assert_eq!(rendered.env["ID"], "created-id");
            } else {
                assert_eq!(rendered.params["mode"], json!([{ "nested": "created-id" }]));
            }
            assert_eq!(
                render(plan.participants().unwrap(), "subject", &BTreeMap::new()).err().unwrap(),
                format!("participant 'subject' {field}: internal error: no creation-time value for '{{{{container.late.container_id}}}}'")
            );
        }
    }

    #[test]
    fn only_pod_template_files_with_deferred_references_are_marked_late() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        for (text, template, deferred) in [
            ("{{self.late.pod_uid}}", true, true),
            ("{{subject.late.container_id}}", true, true),
            ("{{self.late.ip}}", true, true),
            ("{{peer.late.pod_uid}}", true, true),
            ("{{peer.late.container_id}}", true, true),
            ("{{peer.late.ip}}", true, true),
            ("{{container.late.container_id}}", true, false),
            ("{{self.facts.shared_dir}}", true, false),
            ("{{self.late.pod_uid}}", false, false),
            ("{{not a reference}}", false, false),
        ] {
            std::fs::write(source.parent().unwrap().join("input.txt"), text).unwrap();
            let yaml = format!(
                r#"
timeout: 1s
subject_os: [linux]
participants:
  subject:
    type: file-client
    subject: true
    realization: {{kind: kubernetes-pod}}
    config:
      files:
        settings.yaml: {{from: input.txt, template: {template}}}
  peer: {{type: file-client}}
  container: {{type: server, realization: {{kind: linux-container}}}}
{}
"#,
                minimal_procedure!()
            );
            let plan = compile_case(&yaml, &source);
            let rendered = render(
                plan.participants().unwrap(),
                "subject",
                &BTreeMap::from([("container".into(), "created-id".into())]),
            )
            .unwrap();
            assert_eq!(
                rendered.late_files,
                if deferred {
                    BTreeSet::from(["settings.yaml".into()])
                } else {
                    BTreeSet::new()
                },
                "{text}, template: {template}"
            );
            if deferred || !template {
                assert_eq!(rendered.files["settings.yaml"], text.as_bytes());
            }
        }
    }

    #[test]
    fn an_own_ip_reference_in_env_stays_canonical_through_rendering() {
        let plan = compile("{}", Path::new("cases/alpha/config.yaml"));
        let rendered = render(plan.participants().unwrap(), "server", &BTreeMap::new()).unwrap();
        assert_eq!(rendered.env["OWN"], "{{ self.late.ip }}/{{ self.late.ip }}");
        assert!(rendered.late_files.is_empty());
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
        assert_eq!(plan.effective_config("subject").unwrap().files, rendered.files);
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
    fn creation_uses_the_planned_bytes_of_file_and_directory_sources_after_the_sources_change() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir_all(case_dir.join("input/a/empty")).unwrap();
        std::fs::create_dir_all(case_dir.join("input/z/nested")).unwrap();
        std::fs::write(case_dir.join("input/a/data"), "first").unwrap();
        std::fs::write(case_dir.join("input/empty"), "").unwrap();
        std::fs::write(case_dir.join("input/z/data"), "{{not a reference}}").unwrap();
        std::fs::write(case_dir.join("input/z/nested/data"), [0xff, 0, 0xfe]).unwrap();
        std::fs::write(case_dir.join("raw.txt"), b"\xff\0{{self.facts.shared_dir}}").unwrap();
        std::fs::write(case_dir.join("input.txt"), "{{server.late.container_id}}").unwrap();
        let plan = compile(
            "{files: {files/: input, raw.txt: raw.txt, settings.yaml: {from: input.txt, template: true}}}",
            &source,
        );
        let mut expected = plan.effective_config("subject").unwrap().files.clone();
        assert_eq!(
            expected.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "files/a/data",
                "files/empty",
                "files/z/data",
                "files/z/nested/data",
                "raw.txt",
                "settings.yaml"
            ]
        );
        expected.insert("settings.yaml".into(), b"created-id".to_vec());
        let created = BTreeMap::from([("server".into(), "created-id".into())]);

        std::fs::remove_file(case_dir.join("input.txt")).unwrap();
        std::fs::remove_file(case_dir.join("raw.txt")).unwrap();
        std::fs::remove_dir_all(case_dir.join("input")).unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &created).unwrap().files,
            expected
        );

        std::fs::create_dir_all(case_dir.join("raw.txt")).unwrap();
        std::fs::write(case_dir.join("input"), "now a file").unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &created).unwrap().files,
            expected
        );

        std::fs::remove_file(case_dir.join("input")).unwrap();
        std::fs::create_dir_all(case_dir.join("input/changed")).unwrap();
        std::fs::write(case_dir.join("input/changed/data"), "changed").unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &created).unwrap().files,
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_added_to_a_directory_source_after_planning_does_not_change_creation() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir_all(case_dir.join("input/nested")).unwrap();
        std::fs::write(case_dir.join("input/nested/kept"), "kept").unwrap();
        std::fs::create_dir(case_dir.join("outside")).unwrap();
        std::fs::write(case_dir.join("outside/file"), "outside").unwrap();
        let link = case_dir.join("input/nested/link");
        for target in ["../../outside/file", "../../outside", ".", "../../missing"] {
            let plan = compile("{files: {files/: input}}", &source);
            std::os::unix::fs::symlink(target, &link).unwrap();
            assert_eq!(
                try_compile("{files: {files/: input}}", &source).err().unwrap(),
                format!("case 'alpha' in {}: on subject OS 'linux', participant 'subject' file 'files/' ({}) is a symlink; directory sources cannot contain symlinks", source.display(), link.display()),
            );
            assert_eq!(
                render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                    .unwrap()
                    .files,
                BTreeMap::from([("files/nested/kept".into(), b"kept".to_vec())]),
            );
            std::fs::remove_file(&link).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_added_to_a_directory_source_after_planning_does_not_change_creation() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let path = source.parent().unwrap().join("input/socket");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        std::fs::write(path.parent().unwrap().join("kept"), "kept").unwrap();
        let plan = compile("{files: {files/: input}}", &source);
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(
            try_compile("{files: {files/: input}}", &source).err().unwrap(),
            format!("case 'alpha' in {}: on subject OS 'linux', participant 'subject' file 'files/' ({}) is not a regular file or directory", source.display(), path.display()),
        );
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .unwrap()
                .files,
            BTreeMap::from([("files/kept".into(), b"kept".to_vec())]),
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
    fn a_non_utf8_name_added_to_a_directory_source_after_planning_does_not_change_creation() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let directory = source.parent().unwrap().join("input");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("kept"), "kept").unwrap();
        let plan = compile("{files: {files/: input}}", &source);
        std::fs::write(directory.join(OsStr::from_bytes(b"\xff")), "").unwrap();
        assert!(try_compile("{files: {files/: input}}", &source)
            .err()
            .unwrap()
            .ends_with("has a non-UTF-8 name"));
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .unwrap()
                .files,
            BTreeMap::from([("files/kept".into(), b"kept".to_vec())]),
        );
    }

    #[test]
    fn creation_uses_the_planned_bytes_of_a_regular_case_file_after_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let path = source.parent().unwrap().join("input.bin");
        let bytes = b"\xff\0{{not a reference}}";
        std::fs::write(&path, bytes).unwrap();
        let plan = compile("{files: {settings.yaml: input.bin}}", &source);
        assert_eq!(plan.effective_config("subject").unwrap().files["settings.yaml"], bytes);
        let planned = BTreeMap::from([("settings.yaml".to_owned(), bytes.to_vec())]);
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .unwrap()
                .files,
            planned
        );
        std::fs::write(&path, "changed").unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .unwrap()
                .files,
            planned
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .unwrap()
                .files,
            planned
        );
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            render(plan.participants().unwrap(), "subject", &BTreeMap::new())
                .unwrap()
                .files,
            planned
        );
    }
}
