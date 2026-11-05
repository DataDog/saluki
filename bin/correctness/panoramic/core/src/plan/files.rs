//! Reads regular case files and expands directory sources without following nested symlinks.

use std::{collections::BTreeMap, path::Path};

use crate::case::FileSource;

pub(crate) fn read_source(
    participant: &str, name: &str, source: &FileSource, case_source: &Path, files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let path = case_source
        .parent()
        .expect("case sources have a directory")
        .join(&source.from);
    let field = format!("participant '{participant}' file '{name}' ({})", path.display());
    let metadata = std::fs::metadata(&path).map_err(|error| format!("{field}: {error}"))?;
    if name.ends_with('/') {
        if !metadata.is_dir() {
            return Err(format!("{field} is not a directory"));
        }
        return read_directory(participant, name, &path, files);
    }
    if !metadata.is_file() {
        return Err(format!("{field} is not a regular file"));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("{field}: {error}"))?;
    files.insert(name.to_owned(), bytes);
    Ok(())
}

fn read_directory(
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
    use serde_json::Value;

    use super::*;
    use crate::{
        artifact::ArtifactRole,
        case::{
            fixtures::{minimal_procedure, write_case, Client, Server, STEPS},
            CaseLoader, Registries,
        },
        endpoint::{ConsumedEndpoint, Endpoint, Evidence, Port},
        layout::{Os, Realization, RealizationKind},
        participant::{require_early_config_value, CaseValues, EffectiveConfig, FactDeclaration, ParticipantType},
        plan::{CasePlan, Planner},
    };

    struct FileServer;

    impl FileServer {
        fn settings(files: &BTreeMap<String, Vec<u8>>) -> Value {
            serde_saphyr::from_str(simdutf8::basic::from_utf8(&files["settings.yaml"]).unwrap()).unwrap()
        }
    }

    impl ParticipantType for FileServer {
        fn name(&self) -> &'static str {
            "file-server"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            Server.supports()
        }

        fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn config_files(&self) -> &'static [&'static str] {
            &["settings.yaml", "raw.bin", "raw.txt"]
        }

        fn validate_config(&self, _: Realization, config: &CaseValues) -> Result<(), String> {
            let settings = Self::settings(&config.file_contents);
            require_early_config_value(
                "settings.yaml",
                "listen.socket",
                settings["listen"]["socket"].as_str().unwrap(),
            )
        }

        fn default_env(&self, _: Realization, config: &CaseValues) -> BTreeMap<String, String> {
            let mut env = BTreeMap::from([("LEVEL".into(), "info".into())]);
            if Self::settings(&config.file_contents)["listen"]["port"].is_null() {
                env.insert("PORT".into(), "9000".into());
            }
            env
        }

        fn endpoints(&self, _: Realization, config: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
            let settings = Self::settings(&config.files);
            let socket = settings["listen"]["socket"].as_str().unwrap();
            require_early_config_value("settings.yaml", "listen.socket", socket)?;
            let port = config
                .env
                .get("PORT")
                .map(|port| port.parse().unwrap())
                .unwrap_or_else(|| settings["listen"]["port"].as_u64().unwrap().try_into().unwrap());
            Ok(BTreeMap::from([
                ("api".into(), Endpoint::http(Port::Fixed(port))),
                ("socket".into(), Endpoint::unix_stream(socket)),
            ]))
        }

        fn readiness(&self, _: Realization, config: &EffectiveConfig) -> Vec<Evidence> {
            let settings = Self::settings(&config.files);
            assert_eq!(settings["upstream"], "{{ self.connections.upstream.url }}");
            vec![Evidence::file_exists(settings["listen"]["socket"].as_str().unwrap())]
        }

        fn consumes(&self, realization: Realization, config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
            assert_eq!(
                Self::settings(&config.files)["upstream"],
                "{{ self.connections.upstream.url }}"
            );
            Client.consumes(realization, config)
        }

        fn facts(&self, _: Realization) -> &'static [FactDeclaration] {
            &[FactDeclaration {
                name: "port",
                value: "8123",
            }]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    const SETTINGS: &str = concat!(
        "listen:\n  socket: '{{ self.facts.shared_dir }}/input.sock'\n  port: {{self.facts.port}}\n",
        "upstream: '{{ self.connections.upstream.url }}'\n",
        "address: '{{self.endpoints.api.url}}'\n",
        "identity: '{{  peer.late.container_id  }}'\n"
    );

    fn compile(config: &str, source: &Path, os: Os) -> Result<CasePlan, String> {
        let registries = Registries {
            participant_types: &[&FileServer, &Server],
            steps: STEPS,
        };
        let yaml = format!(
            "timeout: 1s\nsubject_os: [{}]\nparticipants:\n  subject: {{type: file-server, subject: true, connect: {{upstream: peer.api}}, config: {config}}}\n  peer: {{type: server}}\n{}",
            os.name(), minimal_procedure!()
        );
        let case = CaseLoader::new(registries)
            .load_content("cases", &yaml, "alpha", source)
            .map_err(|error| error.to_string())?
            .remove(0);
        // Template contents come from load; planning must not reopen them.
        std::fs::remove_file(source.parent().unwrap().join("settings.yaml")).unwrap();
        Planner::with_run(registries, "testrun0")
            .compile(case, os)
            .map_err(|error| error.to_string())
            .map(|compiled| compiled.into_plan().unwrap())
    }

    #[test]
    fn hooks_read_early_file_values_and_the_plan_resolves_addresses_without_changing_late_references() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        std::fs::write(source.parent().unwrap().join("settings.yaml"), SETTINGS).unwrap();
        let raw = b"\xff\0{{self.facts.port}} {{not a reference}}";
        std::fs::write(source.parent().unwrap().join("raw.bin"), raw).unwrap();
        std::fs::write(source.parent().unwrap().join("raw.txt"), SETTINGS).unwrap();
        let plan = compile(
            "{files: {settings.yaml: {from: settings.yaml, template: true}, raw.bin: raw.bin, raw.txt: raw.txt}}",
            &source,
            Os::Linux,
        )
        .unwrap();
        let early = SETTINGS
            .replace("{{ self.facts.shared_dir }}", "/airlock/subject/shared")
            .replace("{{self.facts.port}}", "8123");
        assert_eq!(
            plan.case_values("subject").unwrap().file_contents["settings.yaml"],
            early.as_bytes()
        );
        let effective = plan.effective_config("subject").unwrap();
        assert_eq!(
            effective.files["settings.yaml"],
            early
                .replace("{{ self.connections.upstream.url }}", "http://peer:8080")
                .replace("{{self.endpoints.api.url}}", "http://subject:8123")
                .as_bytes()
        );
        assert_eq!(effective.files["raw.bin"], raw);
        assert_eq!(effective.files["raw.txt"], SETTINGS.as_bytes());
        assert_eq!(
            effective.env,
            BTreeMap::from([
                ("LEVEL".into(), "info".into()),
                ("UPSTREAM_URL".into(), "http://peer:8080".into())
            ])
        );
        assert_eq!(
            plan.create_order(),
            &[vec!["peer".to_string()], vec!["subject".to_string()]]
        );
    }

    #[test]
    fn a_definition_reading_a_non_early_file_value_reports_the_file_key_and_reference() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        std::fs::write(
            source.parent().unwrap().join("settings.yaml"),
            SETTINGS.replace(
                "{{ self.facts.shared_dir }}/input.sock",
                "{{  peer.late.container_id  }}",
            ),
        )
        .unwrap();
        let error = compile(
            "{files: {settings.yaml: {from: settings.yaml, template: true}}}",
            &source,
            Os::Linux,
        )
        .err()
        .unwrap();
        assert_eq!(error, format!(
            "case 'alpha' in {}: on subject OS 'linux', participant 'subject' of type 'file-server' as linux-container on linux has invalid configuration: configuration file 'settings.yaml' key 'listen.socket' contains reference '{{{{  peer.late.container_id  }}}}'; only early facts are substituted before planning",
            source.display()
        ));
    }

    #[test]
    fn case_env_and_os_overlays_still_override_defaults_and_file_values() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        for (env, settings, port, level) in [
            ("{}", SETTINGS.to_string(), "8123", "info"),
            (
                "{LEVEL: debug}",
                SETTINGS.replace("  port: {{self.facts.port}}\n", ""),
                "9000",
                "debug",
            ),
            ("{PORT: '8124'}", SETTINGS.to_string(), "8124", "info"),
        ] {
            std::fs::write(source.parent().unwrap().join("settings.yaml"), &settings).unwrap();
            let plan = compile(
                &format!("{{env: {env}, files: {{settings.yaml: {{from: settings.yaml, template: true}}}}}}"),
                &source,
                Os::Linux,
            )
            .unwrap();
            let config = plan.effective_config("subject").unwrap();
            assert_eq!(config.env["LEVEL"], level);
            assert!(simdutf8::basic::from_utf8(&config.files["settings.yaml"])
                .unwrap()
                .contains(&format!("address: 'http://subject:{port}'")));
            std::fs::write(source.parent().unwrap().join("settings.yaml"), settings).unwrap();
            let plan = compile(&format!("{{env: {env}, files: {{settings.yaml: {{from: settings.yaml, template: true}}}}, os: {{linux: {{env: {{PORT: '8125'}}}}}}}}"), &source, Os::Linux).unwrap();
            assert_eq!(plan.effective_config("subject").unwrap().env["PORT"], "8125");
            assert_eq!(
                plan.endpoint_address("subject", "subject", "api").unwrap().url(),
                "http://subject:8125"
            );
        }
    }
}
