//! Load-time checks of case participants against their registered types.

use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use saluki_error::{generic_error, GenericError};

use super::{
    config_references::{check_config_references, check_param_keys, FileTemplates},
    file::{CaseFile, FileSource, ParticipantEntry},
};
use crate::{
    backend::backend,
    layout::Os,
    participant::{find_type, unknown_type_message, ParticipantType},
};

/// Checks participant names, configuration and connections against registered types.
///
/// # Errors
///
/// Returns the first error in this order: participant names, subject count, subject OS override;
/// then, per participant in name order, registered type, realization options, file names and sources,
/// parameter key syntax then declarations, OS overlays (reachable OS, file names and sources, parameter key syntax
/// then declarations), connection targets (existing participant), and configuration reference syntax
/// and names (base, then overlays; `env`, `params`, then template files). Sources must be nonempty relative paths
/// within the case directory. Names ending in `/` require directories; other names require regular files.
/// Template sources must be regular UTF-8 files; each is read and parsed once. Other sources are not read.
/// Map keys and overlay OSes are checked in sorted order. Every error names the case and source.
pub(super) fn check_participants(
    file: &CaseFile, subject_os: &BTreeSet<Os>, types: &[&dyn ParticipantType], name: &str, source: &Path,
) -> Result<FileTemplates, GenericError> {
    let error = |message: String| generic_error!("case '{}' in {}: {}", name, source.display(), message);
    let case_dir = source.parent().expect("a case source names a file in its directory");
    for participant in file.participants.keys() {
        if !is_dns_label(participant) {
            return Err(error(format!(
                "participant name '{participant}' must be a DNS label: 1 to 63 lowercase letters, digits or '-', \
                 starting with a letter and not ending with '-'"
            )));
        }
        if ["traffic", "details"].contains(&participant.as_str()) {
            return Err(error(format!(
                "participant name '{participant}' is reserved for harness {participant} artifacts; choose another name"
            )));
        }
        if participant == "self" {
            return Err(error(
                "participant name 'self' is reserved for references to the participant itself; choose another name"
                    .into(),
            ));
        }
    }

    let mut subjects = file.participants.iter().filter(|(_, entry)| entry.subject);
    let Some((subject, entry)) = subjects.next() else {
        return Err(error(
            "no participant is the subject; set 'subject: true' on exactly one".into(),
        ));
    };
    if let Some((other, _)) = subjects.next() {
        return Err(error(format!(
            "participants '{subject}' and '{other}' are both marked 'subject: true'; \
             exactly one participant is the subject"
        )));
    }
    if entry.os.is_some() {
        return Err(error(format!(
            "subject '{subject}' sets 'os'; the subject runs on each OS in subject_os, so remove it"
        )));
    }

    let mut sources = BTreeMap::new();
    let mut templates = FileTemplates::default();
    for (participant, entry) in &file.participants {
        let Some(participant_type) = find_type(types, &entry.participant_type) else {
            return Err(error(unknown_type_message(participant, &entry.participant_type, types)));
        };
        let holder = format!("participant '{participant}'");
        let type_name = participant_type.name();
        if let Some(realization) = &entry.realization {
            let kind = realization.kind.name();
            let options = backend(realization.kind).options();
            for (key, value) in &realization.options {
                if !options.iter().any(|option| option.name == key) {
                    let declared = options.iter().map(|option| option.name).collect::<Vec<_>>();
                    return Err(error(format!(
                        "{holder} sets realization option '{key}', which {kind} does not accept; it accepts {}",
                        declared_names(&declared),
                    )));
                }
                if !value.is_boolean() {
                    return Err(error(format!(
                        "{holder} sets realization option '{key}' for {kind} to {value}; expected a boolean"
                    )));
                }
            }
        }
        check_keys(
            entry.config.files.keys(),
            participant_type.config_files(),
            "declares",
            |key| format!("{holder} supplies configuration file '{key}', which type '{type_name}' does not declare"),
        )
        .map_err(&error)?;
        check_file_sources(&entry.config.files, case_dir, &holder, "", &mut sources).map_err(&error)?;
        check_param_keys(&entry.config.params, &format!("{holder} config.params")).map_err(&error)?;
        check_keys(
            entry.config.params.keys(),
            participant_type.params(),
            "accepts",
            |key| format!("{holder} sets parameter '{key}', which type '{type_name}' does not accept"),
        )
        .map_err(&error)?;

        let possible_os = possible_os(entry, subject_os);
        for (os, overlay) in &entry.config.os {
            if !possible_os.contains(os) {
                return Err(error(format!(
                    "{holder} has an 'os' overlay for '{}', but runs only on {}",
                    os.name(),
                    possible_os.iter().map(|os| os.name()).collect::<Vec<_>>().join(", ")
                )));
            }
            let os = os.name();
            check_keys(
                overlay.files.keys(),
                participant_type.config_files(),
                "declares",
                |key| {
                    format!(
                        "{holder} supplies configuration file '{key}', which type '{type_name}' does not declare \
                     in its '{os}' overlay"
                    )
                },
            )
            .map_err(&error)?;
            check_file_sources(
                &overlay.files,
                case_dir,
                &holder,
                &format!(" in its '{os}' overlay"),
                &mut sources,
            )
            .map_err(&error)?;
            check_param_keys(&overlay.params, &format!("{holder} config.os.{os}.params")).map_err(&error)?;
            check_keys(overlay.params.keys(), participant_type.params(), "accepts", |key| {
                format!(
                    "{holder} sets parameter '{key}', which type '{type_name}' does not accept \
                     in its '{os}' overlay"
                )
            })
            .map_err(&error)?;
        }

        for (consumed, target) in &entry.connect {
            if !file.participants.contains_key(&target.participant) {
                return Err(error(format!(
                    "{holder} connects '{consumed}' to unknown participant '{}'",
                    target.participant
                )));
            }
        }
        check_config_references(file, participant, entry, &sources, &mut templates).map_err(&error)?;
    }
    Ok(templates)
}

/// Checks source paths and declared kinds, following top-level symlinks even outside the case directory.
/// Reads only `template: true` files, requiring UTF-8 and caching each path in `sources` once.
fn check_file_sources(
    files: &BTreeMap<String, FileSource>, case_dir: &Path, holder: &str, suffix: &str,
    sources: &mut BTreeMap<PathBuf, String>,
) -> Result<(), String> {
    for (name, source) in files {
        let from = source.from.display();
        if source.from.as_os_str().is_empty() {
            return Err(format!(
                "{holder} supplies configuration file '{name}'{suffix} from an empty path; \
                 write a path relative to the case directory"
            ));
        }
        if !source.from.is_relative() {
            return Err(format!(
                "{holder} supplies configuration file '{name}'{suffix} from absolute path '{from}'; \
                 write a path relative to the case directory"
            ));
        }
        let depth = source
            .from
            .components()
            .try_fold(0usize, |depth, component| match component {
                Component::Normal(_) => Some(depth + 1),
                Component::CurDir => Some(depth),
                Component::ParentDir => depth.checked_sub(1),
                Component::Prefix(_) | Component::RootDir => None,
            });
        if depth.is_none() {
            return Err(format!(
                "{holder} supplies configuration file '{name}'{suffix} from '{from}', which leaves the case directory"
            ));
        }
        let directory = name.ends_with('/');
        if directory && source.template {
            return Err(format!(
                "{holder} supplies configuration directory '{name}'{suffix} from '{from}' with 'template: true'; only a regular file can be a template"
            ));
        }
        let path = case_dir.join(&source.from);
        let metadata = std::fs::metadata(&path).map_err(|_| {
            format!(
                "{holder} supplies configuration file '{name}'{suffix} from '{from}', which does not exist at {}",
                path.display()
            )
        })?;
        if directory && !metadata.is_dir() {
            return Err(format!(
                "{holder} supplies configuration directory '{name}'{suffix} from '{from}', which is not a directory"
            ));
        }
        if !directory && !metadata.is_file() {
            return Err(format!(
                "{holder} supplies configuration file '{name}'{suffix} from '{from}', which is not a regular file"
            ));
        }
        if source.template {
            if let Entry::Vacant(entry) = sources.entry(source.from.clone()) {
                let bytes = std::fs::read(&path).map_err(|error| {
                    format!(
                        "{holder} supplies configuration file '{name}'{suffix} from '{from}', which can't be read: {error}"
                    )
                })?;
                let text = String::from_utf8(bytes).map_err(|_| format!(
                    "{holder} supplies configuration file '{name}'{suffix} from '{from}' with 'template: true', which is not UTF-8 text"
                ))?;
                entry.insert(text);
            }
        }
    }
    Ok(())
}

fn is_dns_label(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.ends_with('-')
}

fn possible_os(entry: &ParticipantEntry, subject_os: &BTreeSet<Os>) -> BTreeSet<Os> {
    match entry.os {
        Some(os) => BTreeSet::from([os]),
        None => subject_os.clone(),
    }
}

fn check_keys<'a>(
    mut keys: impl Iterator<Item = &'a String>, declared: &[&str], verb: &str, describe: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    if let Some(key) = keys.find(|key| !declared.contains(&key.as_str())) {
        return Err(format!("{}; it {verb} {}", describe(key), declared_names(declared)));
    }
    Ok(())
}

pub(crate) fn declared_names(names: &[&str]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        artifact::ArtifactRole,
        case::{
            fixtures::{minimal_procedure, write_case, FixtureTypes, STEPS},
            CaseLoader, CaseSchema, Registries,
        },
        layout::Realization,
    };

    struct Server;

    impl ParticipantType for Server {
        fn name(&self) -> &'static str {
            "server"
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn config_files(&self) -> &'static [&'static str] {
            &["files/", "settings.yaml"]
        }

        fn params(&self) -> &'static [&'static str] {
            &["mode"]
        }

        fn processes(&self) -> &'static [&'static str] {
            &["server"]
        }
    }

    struct Client;

    impl ParticipantType for Client {
        fn name(&self) -> &'static str {
            "client"
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    const TYPES: &[&dyn ParticipantType] = &[&Server, &Client];

    fn case_yaml(participants: &str, subject_os: &str) -> String {
        format!(
            "timeout: 1s\nsubject_os: [{subject_os}]\nparticipants:\n{participants}\n{}\n",
            minimal_procedure!()
        )
    }

    fn check(yaml: &str) -> Result<(), GenericError> {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", yaml);
        std::fs::write(source.parent().unwrap().join("source.yaml"), "").unwrap();
        let file: CaseFile = serde_saphyr::from_str(yaml).unwrap();
        let subject_os = file.check("alpha", &source).unwrap();
        check_participants(&file, &subject_os, TYPES, "alpha", &source).map(|_| ())
    }

    #[track_caller]
    fn check_error(yaml: &str) -> String {
        let message = check(yaml).unwrap_err().to_string();
        assert!(message.starts_with("case 'alpha' in "), "{message}");
        assert!(message.contains("config.yaml: "), "{message}");
        message
    }

    #[test]
    fn participant_names_must_be_dns_labels_that_start_with_a_letter() {
        for name in ["a", "web", "proxy-web", "load2", &"a".repeat(63)] {
            let yaml = case_yaml(&format!("  '{name}': {{type: server, subject: true}}"), "linux");
            check(&yaml).unwrap();
        }
        for name in [
            "",
            "Web",
            "1web",
            "-web",
            "web-",
            "ad_p",
            "ad.p",
            "äd p",
            &"a".repeat(64),
        ] {
            let yaml = case_yaml(&format!("  '{name}': {{type: server, subject: true}}"), "linux");
            let message = check_error(&yaml);
            assert!(
                message.contains(&format!("participant name '{name}' must be a DNS label")),
                "{message}"
            );
            assert!(
                message.contains("starting with a letter and not ending with '-'"),
                "{message}"
            );
        }
    }

    #[test]
    fn the_participant_name_self_is_reserved() {
        let yaml = case_yaml("  self: {type: server, subject: true}", "linux");
        let message = check_error(&yaml);
        assert!(message.contains("participant name 'self' is reserved"), "{message}");
        assert!(message.contains("choose another name"), "{message}");
        check(&yaml.replace("  self:", "  itself:")).unwrap();
    }

    #[test]
    fn harness_artifact_directories_are_reserved_participant_names() {
        for name in ["traffic", "details"] {
            let yaml = case_yaml(&format!("  {name}: {{type: server, subject: true}}"), "linux");
            let message = check_error(&yaml);
            assert!(
                message.contains(&format!(
                    "participant name '{name}' is reserved for harness {name} artifacts; choose another name"
                )),
                "{message}"
            );
            check(&yaml.replace(&format!("  {name}:"), &format!("  {name}-source:"))).unwrap();
        }
    }

    #[test]
    fn a_case_must_mark_exactly_one_subject() {
        let yaml = case_yaml("  b: {type: server}\n  a: {type: client}", "linux");
        let message = check_error(&yaml);
        assert!(
            message.contains("no participant is the subject; set 'subject: true' on exactly one"),
            "{message}"
        );
        check(&yaml.replace("type: server", "type: server, subject: true")).unwrap();

        for participants in [
            "  b: {type: server, subject: true}\n  a: {type: client, subject: true}",
            r#"  c: {type: server, subject: true}
  b: {type: server, subject: true}
  a: {type: client, subject: true}"#,
        ] {
            let yaml = case_yaml(participants, "linux");
            let message = check_error(&yaml);
            assert!(
                message.contains("participants 'a' and 'b' are both marked 'subject: true'"),
                "{message}"
            );
            assert!(!message.contains("'c'"), "{message}");
            check(&yaml.replace("type: server, subject: true", "type: server")).unwrap();
        }
    }

    #[test]
    fn the_subject_cannot_set_an_os() {
        let yaml = case_yaml("  subject: {type: server, subject: true, os: linux}", "linux");
        let message = check_error(&yaml);
        assert!(message.contains("subject 'subject' sets 'os'"), "{message}");
        assert!(
            message.contains("the subject runs on each OS in subject_os, so remove it"),
            "{message}"
        );
        check(&yaml.replace(", os: linux", "")).unwrap();
    }

    #[test]
    fn a_participant_type_must_be_registered() {
        let yaml = case_yaml("  subject: {type: unknown, subject: true}", "linux");
        let message = check_error(&yaml);
        assert!(
            message.contains("participant 'subject' has unknown type 'unknown'; registered types: server, client"),
            "{message}"
        );
        check(&yaml.replace("type: unknown", "type: server")).unwrap();
    }

    #[test]
    fn a_declared_realization_option_with_a_boolean_loads() {
        for value in [false, true] {
            let yaml = case_yaml(
                &format!("  subject: {{type: server, subject: true, realization: {{kind: linux-container, options: {{host_cgroup_namespace: {value}}}}}}}"),
                "linux",
            );
            let cases = loader().load_content(&yaml, "alpha", Path::new("config.yaml")).unwrap();
            assert_eq!(cases.len(), 1);
            let CaseSchema::Participants { file, .. } = cases[0].schema() else {
                panic!("expected participants");
            };
            let realization = file.participants["subject"].realization.as_ref().unwrap();
            assert_eq!(
                realization.options,
                BTreeMap::from([("host_cgroup_namespace".into(), serde_json::Value::Bool(value))])
            );
        }
    }

    #[test]
    fn an_undeclared_realization_option_fails_to_load_naming_the_kind() {
        for (kind, key, declared) in [
            ("linux-container", "unknown", "host_cgroup_namespace"),
            ("linux-container", "host_pid", "host_cgroup_namespace"),
            ("windows-container", "host_cgroup_namespace", "none"),
            ("host-process-group", "host_cgroup_namespace", "none"),
            ("kubernetes-pod", "host_cgroup_namespace", "none"),
        ] {
            let yaml = case_yaml(
                &format!("  subject: {{type: server, subject: true, realization: {{kind: {kind}, options: {{{key}: true}}}}}}"),
                "linux",
            );
            let message = loader()
                .load_content(&yaml, "alpha", Path::new("config.yaml"))
                .err()
                .unwrap()
                .to_string();
            assert_eq!(message, format!(
                "case 'alpha' in config.yaml: participant 'subject' sets realization option '{key}', which {kind} does not accept; it accepts {declared}"
            ));
        }
    }

    #[test]
    fn a_non_boolean_realization_option_fails_to_load() {
        for value in [
            "'true'",
            "'false'",
            "1",
            "0",
            "1.5",
            "null",
            "[]",
            "[true]",
            "{}",
            "{enabled: true}",
        ] {
            let yaml = case_yaml(
                &format!("  subject: {{type: server, subject: true, realization: {{kind: linux-container, options: {{host_cgroup_namespace: {value}}}}}}}"),
                "linux",
            );
            let message = loader()
                .load_content(&yaml, "alpha", Path::new("config.yaml"))
                .err()
                .unwrap()
                .to_string();
            assert!(message.starts_with(
                "case 'alpha' in config.yaml: participant 'subject' sets realization option 'host_cgroup_namespace' for linux-container to "
            ), "{value}: {message}");
            assert!(message.ends_with("; expected a boolean"), "{value}: {message}");
        }
    }

    #[test]
    fn an_empty_realization_options_map_loads() {
        let yaml = super::super::fixtures::PARTICIPANT_FIELDS.replace(
            "subject: true",
            "subject: true, realization: {kind: linux-container, options: {}}",
        );
        let cases = super::super::fixtures::LOADER
            .load_content(&yaml, "alpha", Path::new("config.yaml"))
            .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name(), "alpha");
    }

    #[test]
    fn configuration_names_must_be_declared_by_the_participant_type() {
        for (config, expected, declared) in [
            (
                "{files: {typo: source.yaml}}",
                "supplies configuration file 'typo'",
                "settings.yaml",
            ),
            ("{params: {typo: fast}}", "sets parameter 'typo'", "mode"),
            (
                "{os: {linux: {files: {typo: source.yaml}}}}",
                "supplies configuration file 'typo'",
                "settings.yaml",
            ),
            ("{os: {linux: {params: {typo: fast}}}}", "sets parameter 'typo'", "mode"),
        ] {
            let yaml = case_yaml(
                &format!("  subject: {{type: server, subject: true, config: {config}}}"),
                "linux",
            );
            let message = check_error(&yaml);
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("which type 'server' does not"), "{message}");
            assert!(message.ends_with(declared), "{message}");
            assert_eq!(
                message.contains("in its 'linux' overlay"),
                config.contains("os:"),
                "{message}"
            );
            check(&yaml.replace("typo", declared)).unwrap();
        }

        for (config, expected) in [
            ("{files: {settings.yaml: source.yaml}}", "it declares none"),
            ("{params: {mode: fast}}", "it accepts none"),
            (
                "{os: {linux: {files: {settings.yaml: source.yaml}}}}",
                "it declares none",
            ),
            ("{os: {linux: {params: {mode: fast}}}}", "it accepts none"),
        ] {
            let yaml = case_yaml(
                &format!("  subject: {{type: client, subject: true, config: {config}}}"),
                "linux",
            );
            let message = check_error(&yaml);
            assert!(message.contains("which type 'client' does not"), "{message}");
            assert!(message.ends_with(expected), "{message}");
            check(&yaml.replace(config, "{}")).unwrap();
        }
    }

    const FILE_LOADER: CaseLoader<'static, FixtureTypes> = CaseLoader::new(
        Registries {
            participant_types: TYPES,
            steps: STEPS,
            ..crate::case::fixtures::REGISTRIES
        },
        FixtureTypes,
        "fixture-participants",
    );

    fn file_case(name: &str, from: &Path, template: bool, overlay: bool) -> String {
        let config = format!(
            "{{files: {{{name}: {{from: {}, template: {template}}}}}}}",
            serde_json::to_string(from).unwrap(),
        );
        let config = if overlay {
            format!("{{os: {{linux: {config}}}}}")
        } else {
            config
        };
        case_yaml(
            &format!("  subject: {{type: server, subject: true, config: {config}}}"),
            "linux",
        )
    }

    #[test]
    fn a_configuration_file_source_must_exist_relative_to_the_case_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "").canonicalize().unwrap();
        let case_dir = source.parent().unwrap();
        std::fs::write(case_dir.join("settings.yaml"), "").unwrap();
        std::fs::create_dir(case_dir.join("scripts")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("missing.yaml", case_dir.join("dangling.yaml")).unwrap();
            let external = dir.path().join("external.yaml");
            std::fs::write(&external, "").unwrap();
            std::os::unix::fs::symlink(external, case_dir.join("linked.yaml")).unwrap();
        }

        for overlay in [false, true] {
            let suffix = if overlay { " in its 'linux' overlay" } else { "" };
            let missing_paths = [
                "missing.yaml",
                #[cfg(unix)]
                "dangling.yaml",
            ];
            for from in missing_paths {
                let yaml = file_case("settings.yaml", Path::new(from), false, overlay);
                let message = FILE_LOADER
                    .load_content(&yaml, "alpha", &source)
                    .err()
                    .unwrap()
                    .to_string();
                assert_eq!(message, format!(
                    "case 'alpha' in {}: participant 'subject' supplies configuration file 'settings.yaml'{suffix} from '{from}', which does not exist at {}",
                    source.display(), case_dir.join(from).display(),
                ));
            }
            for from in [
                "settings.yaml",
                "scripts/../settings.yaml",
                #[cfg(unix)]
                "linked.yaml",
            ] {
                let yaml = file_case("settings.yaml", Path::new(from), false, overlay);
                std::fs::write(&source, yaml).unwrap();
                let cases = FILE_LOADER.load_file(&source).unwrap();
                assert_eq!(cases.len(), 1);
                assert_eq!(cases[0].source(), source);
            }
        }
    }

    #[test]
    fn a_directory_name_requires_a_directory_source_and_a_file_name_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::write(case_dir.join("input"), "").unwrap();
        std::fs::create_dir(case_dir.join("directory")).unwrap();

        for overlay in [false, true] {
            let suffix = if overlay { " in its 'linux' overlay" } else { "" };
            for (name, from, template, reason) in [
                ("files/", "directory", false, None),
                ("settings.yaml", "input", false, None),
                ("settings.yaml", "input", true, None),
                ("files/", "input", false, Some("directory")),
                ("settings.yaml", "directory", false, Some("regular file")),
                ("settings.yaml", "directory", true, Some("regular file")),
            ] {
                let yaml = file_case(name, Path::new(from), template, overlay);
                let loaded = FILE_LOADER.load_content(&yaml, "alpha", &source);
                if let Some(reason) = reason {
                    let kind = if name.ends_with('/') { "directory" } else { "file" };
                    assert_eq!(
                        loaded.err().unwrap().to_string(),
                        format!(
                            "case 'alpha' in {}: participant 'subject' supplies configuration {kind} '{name}'{suffix} from '{from}', which is not a {reason}",
                            source.display(),
                        ),
                    );
                } else {
                    assert_eq!(loaded.unwrap().len(), 1);
                }
            }
        }
    }

    #[test]
    fn a_template_directory_source_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::write(case_dir.join("input"), "").unwrap();
        std::fs::create_dir(case_dir.join("directory")).unwrap();

        for overlay in [false, true] {
            let suffix = if overlay { " in its 'linux' overlay" } else { "" };
            for from in ["directory", "input", "missing"] {
                let yaml = file_case("files/", Path::new(from), true, overlay);
                assert_eq!(
                    FILE_LOADER.load_content(&yaml, "alpha", &source).err().unwrap().to_string(),
                    format!(
                        "case 'alpha' in {}: participant 'subject' supplies configuration directory 'files/'{suffix} from '{from}' with 'template: true'; only a regular file can be a template",
                        source.display(),
                    ),
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn configuration_sources_follow_top_level_symlinks_to_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let case_dir = source.parent().unwrap();
        std::fs::create_dir(dir.path().join("external")).unwrap();
        std::fs::write(dir.path().join("external/input"), "").unwrap();
        std::os::unix::fs::symlink("../external", case_dir.join("directory")).unwrap();
        std::os::unix::fs::symlink("../external/input", case_dir.join("input")).unwrap();
        for overlay in [false, true] {
            for (name, from, template) in [
                ("files/", "directory", false),
                ("settings.yaml", "input", false),
                ("settings.yaml", "input", true),
            ] {
                let yaml = file_case(name, Path::new(from), template, overlay);
                let loaded = FILE_LOADER.load_content(&yaml, "alpha", &source).unwrap();
                assert_eq!(loaded.len(), 1);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn special_configuration_sources_fail_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "");
        let _socket = std::os::unix::net::UnixListener::bind(source.parent().unwrap().join("socket")).unwrap();
        for overlay in [false, true] {
            let suffix = if overlay { " in its 'linux' overlay" } else { "" };
            for (name, template, kind, reason) in [
                ("files/", false, "directory", "directory"),
                ("settings.yaml", false, "file", "regular file"),
                ("settings.yaml", true, "file", "regular file"),
            ] {
                let yaml = file_case(name, Path::new("socket"), template, overlay);
                assert_eq!(
                    FILE_LOADER.load_content(&yaml, "alpha", &source).err().unwrap().to_string(),
                    format!(
                        "case 'alpha' in {}: participant 'subject' supplies configuration {kind} '{name}'{suffix} from 'socket', which is not a {reason}",
                        source.display(),
                    ),
                );
            }
        }
    }

    #[test]
    fn a_configuration_file_source_must_not_be_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "").canonicalize().unwrap();
        let from = source.parent().unwrap().join("settings.yaml");
        std::fs::write(&from, "").unwrap();
        for overlay in [false, true] {
            let yaml = file_case("settings.yaml", &from, false, overlay);
            let suffix = if overlay { " in its 'linux' overlay" } else { "" };
            let message = FILE_LOADER
                .load_content(&yaml, "alpha", &source)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(message, format!(
                "case 'alpha' in {}: participant 'subject' supplies configuration file 'settings.yaml'{suffix} from absolute path '{}'; write a path relative to the case directory",
                source.display(), from.display(),
            ));
        }
    }

    #[test]
    fn a_configuration_file_source_must_be_nonempty_and_stay_within_the_case_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", "").canonicalize().unwrap();
        std::fs::write(dir.path().join("outside.yaml"), "").unwrap();
        std::fs::create_dir(source.parent().unwrap().join("nested")).unwrap();
        for overlay in [false, true] {
            let suffix = if overlay { " in its 'linux' overlay" } else { "" };
            for from in ["", "..", "../outside.yaml", "nested/../../outside.yaml"] {
                let yaml = file_case("settings.yaml", Path::new(from), false, overlay);
                let message = FILE_LOADER
                    .load_content(&yaml, "alpha", &source)
                    .err()
                    .unwrap()
                    .to_string();
                let reason = if from.is_empty() {
                    "from an empty path; write a path relative to the case directory".to_string()
                } else {
                    format!("from '{from}', which leaves the case directory")
                };
                assert_eq!(message, format!(
                    "case 'alpha' in {}: participant 'subject' supplies configuration file 'settings.yaml'{suffix} {reason}",
                    source.display(),
                ));
            }
        }
    }

    #[test]
    fn a_participant_type_that_declares_only_its_name_and_processes_accepts_no_files_or_params() {
        struct Minimal;

        impl ParticipantType for Minimal {
            fn name(&self) -> &'static str {
                "minimal"
            }

            fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
                &[]
            }

            fn processes(&self) -> &'static [&'static str] {
                &["main"]
            }
        }

        let types: &[&dyn ParticipantType] = &[&Minimal, &Client];
        let source = Path::new("config.yaml");
        for (participants, expected) in [
            (
                "  subject: {type: minimal, subject: true, config: {files: {settings.yaml: missing.yaml}}}",
                "participant 'subject' supplies configuration file 'settings.yaml', which type 'minimal' does not declare; it declares none",
            ),
            (
                "  subject: {type: minimal, subject: true, config: {params: {mode: fast}}}",
                "participant 'subject' sets parameter 'mode', which type 'minimal' does not accept; it accepts none",
            ),
        ] {
            let file: CaseFile = serde_saphyr::from_str(&case_yaml(participants, "linux")).unwrap();
            let subject_os = file.check("alpha", source).unwrap();
            let message = check_participants(&file, &subject_os, types, "alpha", source)
                .unwrap_err()
                .to_string();
            assert_eq!(message, format!("case 'alpha' in config.yaml: {expected}"));
        }
    }

    #[test]
    fn an_os_overlay_must_name_an_os_the_participant_runs_on() {
        for (participants, subject_os, expected_os, valid_os, invalid_os) in [
            (
                "  subject: {type: server, subject: true, config: {os: {OVERLAY: {}}}}",
                "linux, macos",
                "linux, macos",
                "macos",
                "windows",
            ),
            (
                r#"  subject: {type: server, subject: true}
  other: {type: client, os: linux, config: {os: {OVERLAY: {}}}}"#,
                "macos",
                "linux",
                "linux",
                "macos",
            ),
            (
                r#"  subject: {type: server, subject: true}
  other: {type: client, config: {os: {OVERLAY: {}}}}"#,
                "macos",
                "macos",
                "macos",
                "linux",
            ),
        ] {
            let yaml = case_yaml(participants, subject_os);
            let message = check_error(&yaml.replace("OVERLAY", invalid_os));
            assert!(
                message.contains(&format!(
                    "has an 'os' overlay for '{invalid_os}', but runs only on {expected_os}"
                )),
                "{message}"
            );
            check(&yaml.replace("OVERLAY", valid_os)).unwrap();
        }
    }

    #[test]
    fn a_connection_must_name_an_existing_participant_including_itself() {
        let yaml = case_yaml(
            "  client: {type: client, subject: true, connect: {upstream: server.api}}\n  server: {type: server}",
            "linux",
        );
        let message = check_error(&yaml.replace("server.api", "missing.api"));
        assert_eq!(
            message.split_once("config.yaml: ").unwrap().1,
            "participant 'client' connects 'upstream' to unknown participant 'missing'"
        );
        check(&yaml).unwrap();
        check(&yaml.replace("server.api", "client.api")).unwrap();
    }

    #[test]
    fn checks_report_the_first_problem_in_a_fixed_order() {
        let mut yaml = case_yaml(
            r#"  z_bad: {type: unknown, subject: true}
  a:
    type: unknown
    subject: true
    os: linux
    realization: {kind: linux-container, options: {z-option: true, a-option: true, host_cgroup_namespace: []}}
    config:
      files: {z-file: z, a-file: a, settings.yaml: missing-base.yaml}
      params: {z-param: true, a-param: true}
      os:
        windows: {}
        macos: {files: {mac-file: x}}
        linux:
          files: {z-overlay-file: z, a-overlay-file: a, settings.yaml: missing-overlay.yaml}
          params: {z-overlay-param: true, a-overlay-param: true}
    connect: {z-connect: missing.api, a-connect: missing.api}"#,
            "linux, macos",
        );
        for (bad, fix, expected) in [
            ("z_bad", "z", "participant name 'z_bad' must be a DNS label"),
            (
                "z: {type: unknown, subject: true}",
                "z: {type: unknown}",
                "participants 'a' and 'z' are both marked",
            ),
            ("    os: linux\n", "", "subject 'a' sets 'os'"),
            (
                "    type: unknown",
                "    type: server",
                "participant 'a' has unknown type",
            ),
            (", a-option: true", "", "sets realization option 'a-option'"),
            (", host_cgroup_namespace: []", "", "expected a boolean"),
            ("z-option: true", "", "sets realization option 'z-option'"),
            (", a-file: a", "", "configuration file 'a-file'"),
            ("z-file: z, ", "", "configuration file 'z-file'"),
            (
                "settings.yaml: missing-base.yaml",
                "",
                "configuration file 'settings.yaml' from 'missing-base.yaml'",
            ),
            (", a-param: true", "", "sets parameter 'a-param'"),
            ("z-param: true", "", "sets parameter 'z-param'"),
            (", a-overlay-file: a", "", "configuration file 'a-overlay-file'"),
            ("z-overlay-file: z, ", "", "configuration file 'z-overlay-file'"),
            (
                "settings.yaml: missing-overlay.yaml",
                "",
                "configuration file 'settings.yaml' in its 'linux' overlay from 'missing-overlay.yaml'",
            ),
            (", a-overlay-param: true", "", "sets parameter 'a-overlay-param'"),
            ("z-overlay-param: true", "", "sets parameter 'z-overlay-param'"),
            ("mac-file: x", "", "configuration file 'mac-file'"),
            ("        windows: {}\n", "", "has an 'os' overlay for 'windows'"),
            (
                ", a-connect: missing.api",
                "",
                "connects 'a-connect' to unknown participant 'missing'",
            ),
            (
                "z-connect: missing.api",
                "",
                "connects 'z-connect' to unknown participant 'missing'",
            ),
            (
                "z: {type: unknown}",
                "z: {type: server}",
                "participant 'z' has unknown type",
            ),
        ] {
            let message = check_error(&yaml);
            assert!(message.contains(expected), "expected {expected}: {message}");
            assert!(yaml.contains(bad), "missing replacement: {bad}");
            yaml = yaml.replace(bad, fix);
        }
        check(&yaml).unwrap();

        let yaml = case_yaml(
            r#"  subject:
    type: server
    subject: true
    config: {os: {windows: {files: {typo: x}, params: {typo: true}}}}"#,
            "linux",
        );
        let message = check_error(&yaml);
        assert!(message.contains("has an 'os' overlay for 'windows'"), "{message}");
        check(&yaml.replace("windows: {files: {typo: x}, params: {typo: true}}", "linux: {}")).unwrap();

        let yaml = case_yaml(
            "  z: {type: server}\n  a: {type: client, subject: true, connect: {typo: missing.api}}",
            "linux",
        );
        let message = check_error(&yaml);
        assert!(message.contains("unknown participant 'missing'"), "{message}");
        check(&yaml.replace("missing.api", "a.typo")).unwrap();
        check(&yaml.replace("missing.api", "z.typo")).unwrap();
    }

    fn loader() -> CaseLoader<'static, FixtureTypes> {
        CaseLoader::new(
            Registries {
                participant_types: TYPES,
                steps: STEPS,
                ..crate::case::fixtures::REGISTRIES
            },
            FixtureTypes,
            "fixture",
        )
    }

    fn template_case(config: &str) -> String {
        case_yaml(
            &format!("  subject: {{type: server, subject: true, config: {config}, connect: {{link: other.api}}}}\n  other: {{type: server}}"),
            "linux, windows",
        )
    }

    #[track_caller]
    fn template_error(config: &str, content: &[u8], expected: &str) {
        let dir = tempfile::tempdir().unwrap();
        let source = write_case(dir.path(), "alpha", &template_case(config));
        std::fs::write(source.parent().unwrap().join("source.yaml"), content).unwrap();
        let loader = loader();
        assert_eq!(
            loader.load_file(&source).err().unwrap().to_string(),
            format!(
                "case 'alpha' in {}: participant 'subject' {expected}",
                source.canonicalize().unwrap().display()
            )
        );
    }

    #[test]
    fn template_files_require_utf8_text_and_valid_references_in_the_base_and_overlays() {
        for (content, reason) in [
            (&[0xff, 0xfe][..], "supplies configuration file 'settings.yaml'SUFFIX from 'source.yaml' with 'template: true', which is not UTF-8 text"),
            (&b"{{ x"[..], "PREFIX.files 'settings.yaml' unterminated template reference '{{ x': expected '}}'"),
            (&b"{{ missing.facts.file }}"[..], "PREFIX.files 'settings.yaml' '{{ missing.facts.file }}' references fact 'file' of unknown participant 'missing'; participants: other, subject"),
            (&b"{{ missing.late.ip }}"[..], "PREFIX.files 'settings.yaml' '{{ missing.late.ip }}' references late fact 'ip' of unknown participant 'missing'; participants: other, subject"),
            (&b"{{ missing.endpoints.api.url }}"[..], "PREFIX.files 'settings.yaml' '{{ missing.endpoints.api.url }}' references endpoint 'api' of unknown participant 'missing'; participants: other, subject"),
            (&b"{{ self.connections.missing.url }}"[..], "PREFIX.files 'settings.yaml' '{{ self.connections.missing.url }}' references connection 'missing', which it does not connect; it connects link"),
        ] {
            let values = "{files: {settings.yaml: {from: source.yaml, template: true}}}";
            for (config, prefix, suffix) in [
                (values.to_string(), "config", ""),
                (format!("{{os: {{windows: {values}}}}}"), "config.os.windows", " in its 'windows' overlay"),
            ] {
                template_error(&config, content, &reason.replace("PREFIX", prefix).replace("SUFFIX", suffix));
            }
        }
    }

    #[test]
    fn regular_file_sources_are_read_at_load_only_when_templated() {
        let dir = tempfile::tempdir().unwrap();
        let loader = loader();
        let source = write_case(dir.path(), "alpha", &template_case("{}"));
        std::fs::write(source.parent().unwrap().join("binary"), [0xff, 0xfe]).unwrap();
        std::fs::write(source.parent().unwrap().join("literal"), "{{ x").unwrap();
        std::fs::write(source.parent().unwrap().join("empty"), "").unwrap();
        for from in ["binary", "literal", "empty"] {
            let config = format!("{{files: {{settings.yaml: {from}}}}}");
            let loaded = loader.load_content(&template_case(&config), "alpha", &source).unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].name(), "alpha");
        }
        let config = "{files: {settings.yaml: {from: empty, template: true}}}";
        let loaded = loader.load_content(&template_case(config), "alpha", &source).unwrap();
        assert_eq!(loaded[0].file_templates().get(Path::new("empty")), &"".parse().unwrap());
    }

    #[test]
    fn template_sources_are_read_once_per_written_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut sources = BTreeMap::new();
        let files = BTreeMap::from([(
            "settings.yaml".into(),
            FileSource {
                from: "source.yaml".into(),
                template: true,
            },
        )]);
        std::fs::write(dir.path().join("source.yaml"), "{{ self.facts.file }}").unwrap();
        check_file_sources(&files, dir.path(), "participant 'other'", "", &mut sources).unwrap();
        std::fs::write(dir.path().join("source.yaml"), [0xff, 0xfe]).unwrap();
        check_file_sources(&files, dir.path(), "participant 'subject'", "", &mut sources).unwrap();
        check_file_sources(
            &files,
            dir.path(),
            "participant 'subject'",
            " in its 'windows' overlay",
            &mut sources,
        )
        .unwrap();
        assert_eq!(
            sources,
            BTreeMap::from([("source.yaml".into(), "{{ self.facts.file }}".into())])
        );
    }

    #[test]
    fn template_reference_checks_follow_env_and_params_then_overlays() {
        let mut config = "{env: {KEY: '{{ env'}, params: {mode: '{{ param'}, files: {settings.yaml: {from: source.yaml, template: true}}, os: {windows: {env: {KEY: '{{ overlay'}}}}".to_string();
        for (text, field) in [("{{ env", "config.env 'KEY'"), ("{{ param", "config.params 'mode'")] {
            template_error(
                &config,
                b"{{ file",
                &format!("{field} unterminated template reference '{text}': expected '}}}}'"),
            );
            config = config.replace(text, "literal");
        }
        template_error(
            &config,
            b"{{ file",
            "config.files 'settings.yaml' unterminated template reference '{{ file': expected '}}'",
        );
        template_error(
            &config,
            b"literal",
            "config.os.windows.env 'KEY' unterminated template reference '{{ overlay': expected '}}'",
        );
    }

    #[test]
    fn a_shared_template_checks_connections_for_each_holder() {
        let dir = tempfile::tempdir().unwrap();
        let config = "{files: {settings.yaml: {from: source.yaml, template: true}}}";
        let yaml = template_case(config)
            .replace(
                "other: {type: server}",
                &format!("other: {{type: server, config: {config}, connect: {{link: subject.api}}}}"),
            )
            .replace(", connect: {link: other.api}", "");
        let source = write_case(dir.path(), "alpha", &yaml);
        std::fs::write(
            source.parent().unwrap().join("source.yaml"),
            "{{ self.connections.link.url }}",
        )
        .unwrap();
        let loader = loader();
        assert_eq!(loader.load_file(&source).err().unwrap().to_string(),
            format!("case 'alpha' in {}: participant 'subject' config.files 'settings.yaml' '{{{{ self.connections.link.url }}}}' references connection 'link', which it does not connect; it connects none", source.canonicalize().unwrap().display()));
    }
}
