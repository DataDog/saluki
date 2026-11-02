//! Run-wide isolation scope allocation and shared filesystem paths.

use crate::{layout::Os, ledger::random_id};

/// Allocates case isolation scopes under one run identity.
///
/// Each run has up to 10000 scopes. Fixed-width paths keep Unix socket path budgets independent of allocation order.
pub(crate) struct ScopeRegistry {
    run: String,
    next: u16,
}

impl ScopeRegistry {
    /// Creates a registry with a random eight-character run identity.
    pub(crate) fn new() -> Self {
        Self {
            run: random_id(),
            next: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_run(run: &str) -> Self {
        assert_eq!(run.len(), 8);
        Self {
            run: run.into(),
            next: 0,
        }
    }

    /// Allocates the next scope, or fails when this run's 10000 scopes are allocated.
    pub(crate) fn allocate(&mut self) -> Result<ScopeLayout, String> {
        if self.next == 10000 {
            return Err("all 10000 isolation scopes are allocated".into());
        }
        let scope = ScopeLayout {
            run: self.run.clone(),
            index: self.next,
        };
        self.next += 1;
        Ok(scope)
    }

    /// Returns index 0 of this run for validating a case excluded by OS selection.
    /// Its paths have the same length as allocated scopes.
    pub(crate) fn unallocated(&self) -> ScopeLayout {
        ScopeLayout {
            run: self.run.clone(),
            index: 0,
        }
    }
}

/// A case's run identity and isolation scope index.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScopeLayout {
    pub(crate) run: String,
    pub(crate) index: u16,
}

impl ScopeLayout {
    /// The scope's isolation group ID: the run ID and a four-digit index.
    pub(crate) fn id(&self) -> String {
        format!("{}-{:04}", self.run, self.index)
    }

    /// Renders the scope's host state directory in the participant OS's path style.
    pub(crate) fn state_dir(&self, os: Os) -> String {
        // Fixed roots leave room for socket names even on hosts with long temporary-directory paths.
        match os {
            Os::Linux | Os::MacOs => format!("/tmp/panoramic-{}/{:04}", self.run, self.index),
            Os::Windows => format!(r"C:\Windows\Temp\panoramic-{}\{:04}", self.run, self.index),
        }
    }
}

/// Renders a participant's socket directory beneath the shared filesystem root.
pub(super) fn participant_shared_dir(root: &str, participant: &str, os: Os) -> String {
    let separator = if os == Os::Windows { '\\' } else { '/' };
    format!("{root}{separator}{participant}{separator}shared")
}

/// Checks that a socket another participant consumes lies under its server's `shared_dir` and fits the OS's
/// socket path limit, checking location first. On failure, returns the text following the path in the caller's error.
pub(super) fn check_consumed_socket(path: &str, shared_dir: &str, os: Os) -> Result<(), String> {
    let separator = if os == Os::Windows { '\\' } else { '/' };
    let under = path
        .strip_prefix(shared_dir)
        .and_then(|rest| rest.strip_prefix(separator))
        .is_some_and(|rest| {
            let valid = |component: &str| !matches!(component, "" | "." | "..");
            if os == Os::Windows {
                rest.split(['\\', '/']).all(valid)
            } else {
                rest.split(separator).all(valid)
            }
        });
    if !under {
        return Err(format!(
            ", outside its shared_dir '{shared_dir}'; consumed sockets must lie under their server's shared_dir"
        ));
    }
    let limit = match os {
        Os::MacOs => 103,
        Os::Linux | Os::Windows => 107,
    };
    if path.len() > limit {
        return Err(format!(
            " ({} bytes); a socket path on {} holds at most {limit} bytes",
            path.len(),
            os.name()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        artifact::ArtifactRole,
        case::{
            fixtures::{minimal_procedure, FixtureTypes, PARTICIPANT_FIELDS, STEPS},
            CaseLoader, CaseSchema, ProcedureNode, Registries,
        },
        layout::{Realization, RealizationKind},
        participant::{FactDeclaration, ParticipantType, SHARED_DIR_FACT},
        plan::{
            fixtures::{compile, compile_files, compile_with, planner, socket_case, EveryKindServer, FILE_CONFIG},
            Planner,
        },
        step::Need,
    };

    #[test]
    fn a_scope_id_joins_the_run_and_a_four_digit_index() {
        for (index, expected) in [(0, "abcd1234-0000"), (42, "abcd1234-0042"), (9999, "abcd1234-9999")] {
            let scope = ScopeLayout {
                run: "abcd1234".into(),
                index,
            };
            assert_eq!(scope.id(), expected);
        }
    }

    #[test]
    fn scopes_are_numbered_in_allocation_order_under_one_run_directory() {
        let mut scopes = ScopeRegistry::with_run("testrun0");
        for index in 0..3 {
            let scope = scopes.allocate().unwrap();
            assert_eq!(
                scope,
                ScopeLayout {
                    run: "testrun0".into(),
                    index
                }
            );
            assert_eq!(
                scope.state_dir(Os::Linux),
                format!("/tmp/panoramic-testrun0/{index:04}")
            );
        }
    }

    #[test]
    fn an_exhausted_scope_registry_reports_its_capacity() {
        let mut scopes = ScopeRegistry {
            run: "testrun0".into(),
            next: 9999,
        };
        assert_eq!(scopes.allocate().unwrap().index, 9999);
        for _ in 0..2 {
            assert_eq!(
                scopes.allocate(),
                Err("all 10000 isolation scopes are allocated".into())
            );
        }
    }

    #[test]
    fn a_new_registry_uses_an_eight_character_lowercase_run_id() {
        let mut scopes = ScopeRegistry::new();
        let scope = scopes.allocate().unwrap();
        assert_eq!(scope.run.len(), 8);
        assert!(scope
            .run
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()));
        assert_eq!(scope.index, 0);
    }

    #[test]
    fn host_state_directories_use_the_participant_os_path_style() {
        let scope = ScopeLayout {
            run: "testrun0".into(),
            index: 3,
        };
        for (os, expected) in [
            (Os::Linux, "/tmp/panoramic-testrun0/0003"),
            (Os::MacOs, "/tmp/panoramic-testrun0/0003"),
            (Os::Windows, r"C:\Windows\Temp\panoramic-testrun0\0003"),
        ] {
            assert_eq!(scope.state_dir(os), expected);
        }
    }

    #[test]
    fn the_unix_state_directory_is_28_bytes_for_every_index() {
        let mut scopes = ScopeRegistry::with_run("testrun0");
        for _ in 0..10000 {
            let scope = scopes.allocate().unwrap();
            for os in [Os::Linux, Os::MacOs] {
                assert_eq!(scope.state_dir(os).len(), 28);
            }
        }
        assert!(scopes.allocate().is_err());
    }

    #[test]
    fn socket_path_limits_are_103_bytes_on_macos_and_107_elsewhere() {
        for (os, dir, separator, limit) in [
            (Os::MacOs, "/shared", '/', 103),
            (Os::Linux, "/shared", '/', 107),
            (Os::Windows, r"C:\shared", '\\', 107),
        ] {
            let path = format!("{dir}{separator}{}", "x".repeat(limit - dir.len() - 1));
            assert_eq!(check_consumed_socket(&path, dir, os), Ok(()));
            for suffix in ["x", "é"] {
                let path = format!("{path}{suffix}");
                assert_eq!(
                    check_consumed_socket(&path, dir, os),
                    Err(format!(
                        " ({} bytes); a socket path on {} holds at most {limit} bytes",
                        path.len(),
                        os.name()
                    ))
                );
            }
        }
    }

    #[test]
    fn a_path_lies_under_a_directory_only_below_a_separator_without_dot_components() {
        for (os, dir, path, valid) in [
            (Os::Linux, "/shared", "/shared/x.sock", true),
            (Os::Linux, "/shared", "/shared/nested/x.sock", true),
            (Os::Linux, "/shared", "/shared", false),
            (Os::Linux, "/shared", "/shared/", false),
            (Os::Linux, "/shared", "/shared2/x.sock", false),
            (Os::Linux, "/shared", "/shared/../x.sock", false),
            (Os::Linux, "/shared", "/shared/./x.sock", false),
            (Os::Linux, "/shared", "/shared//x.sock", false),
            (Os::Linux, "/shared", "/shared/x.sock/", false),
            (Os::Windows, r"C:\shared", r"C:\shared\x.sock", true),
            (Os::Windows, r"C:\shared", r"C:\shared/x.sock", false),
            (Os::Windows, r"C:\shared", r"C:\shared\..\x.sock", false),
            // Mixed Windows separators can hide parent traversal from a backslash-only split.
            (Os::Windows, r"C:\shared", r"C:\shared\a/..\x.sock", false),
            (Os::Windows, r"C:\shared", r"C:\shared\a/b.sock", true),
            (Os::Windows, r"C:\shared", r"C:\shared\.\x.sock", false),
            (Os::Windows, r"C:\shared", r"C:\shared\\x.sock", false),
        ] {
            let expected = if valid {
                Ok(())
            } else {
                Err(format!(
                    ", outside its shared_dir '{dir}'; consumed sockets must lie under their server's shared_dir"
                ))
            };
            assert_eq!(check_consumed_socket(path, dir, os), expected, "{path}");
        }
        let path = format!("/elsewhere/{}", "x".repeat(108));
        assert!(check_consumed_socket(&path, "/shared", Os::Linux)
            .unwrap_err()
            .contains("outside its shared_dir"));
    }

    #[test]
    fn every_participant_has_a_shared_dir_fact_for_its_realization() {
        for (os, kind, expected) in [
            (Os::Linux, "linux-container", "/airlock/subject/shared"),
            (Os::Windows, "windows-container", r"C:\airlock\subject\shared"),
            (
                Os::MacOs,
                "host-process-group",
                "/tmp/panoramic-testrun0/0000/subject/shared",
            ),
            (
                Os::Linux,
                "host-process-group",
                "/tmp/panoramic-testrun0/0000/subject/shared",
            ),
            (Os::Linux, "kubernetes-pod", "/airlock/subject/shared"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n  subject: {{type: every-kind-server, subject: true, realization: {{kind: {kind}}}}}\n  other: {{type: every-kind-client}}\n{}", os.name(), minimal_procedure!());
            let plan = compile(&yaml, os).unwrap().unwrap();
            assert_eq!(plan.fact("subject", SHARED_DIR_FACT), Some(expected));
            assert_eq!(
                plan.fact("other", SHARED_DIR_FACT),
                Some(
                    expected
                        .replace("/subject/", "/other/")
                        .replace(r"\subject\", r"\other\")
                        .as_str()
                )
            );
        }
    }

    #[test]
    fn a_type_declaring_shared_dir_fails_compile_naming_the_type_and_realization() {
        struct ReservedFact;
        impl ParticipantType for ReservedFact {
            fn name(&self) -> &'static str {
                "reserved"
            }
            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                EveryKindServer.supports()
            }

            fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
                &[]
            }

            fn facts(&self, _: Realization) -> &'static [FactDeclaration] {
                &[FactDeclaration {
                    name: SHARED_DIR_FACT,
                    value: "/wrong",
                }]
            }
            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }
        let registries = Registries {
            participant_types: &[&ReservedFact, &EveryKindServer],
            steps: STEPS,
            ..Default::default()
        };
        let loader = CaseLoader::new(registries, FixtureTypes, "fixture");
        for (os, kind) in [
            (Os::Linux, "linux-container"),
            (Os::MacOs, "host-process-group"),
            (Os::Windows, "windows-container"),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n  a: {{type: every-kind-server, subject: true, config: {{env: {{KEY: '{{{{ self.facts.missing }}}}'}}}}}}\n  z: {{type: reserved}}\n{}", os.name(), minimal_procedure!());
            let case = loader
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let error = Planner::with_run(registries, "testrun0")
                .compile(case, os)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(error, format!("case 'alpha' in cases/alpha/config.yaml: on subject OS '{}', participant 'z' of type 'reserved' as {kind} on {} declares fact 'shared_dir', which panoramic provides for every participant", os.name(), os.name()));
        }
    }

    #[test]
    fn shared_dir_substitutes_into_env_params_and_served_socket_paths() {
        let yaml = concat!("timeout: 1s\nsubject_os: [linux, macos]\nparticipants:\n  client: {type: file-server, subject: true, connect: {target: server.stream}, config: {env: {COPY: '{{ server.facts.shared_dir }}', PORT_OFFSET: '100'}, params: {mode: [{nested: '{{ server.facts.shared_dir }}'}]}}}\n  server: {type: file-server, config: {env: {SOCKET_PATH: '{{ self.facts.shared_dir }}/stream.sock'}}}\n", minimal_procedure!());
        for (os, dir) in [
            (Os::Linux, "/airlock/server/shared"),
            (Os::MacOs, "/tmp/panoramic-testrun0/0000/server/shared"),
        ] {
            let plan = compile(yaml, os).unwrap().unwrap();
            let config = plan.effective_config("client").unwrap();
            assert_eq!(config.env["COPY"], dir);
            assert_eq!(config.params["mode"][0]["nested"], dir);
            assert_eq!(config.env["TARGET"], format!("unix://{dir}/stream.sock"));
            assert_eq!(
                plan.effective_config("server").unwrap().env["SOCKET_PATH"],
                format!("{dir}/stream.sock")
            );
        }
        let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-server, subject: true, config: {FILE_CONFIG}}}\n  other: {{type: server}}\n{}", minimal_procedure!());
        let plan = compile_files(
            &yaml,
            &[(
                "source.yaml",
                "self: {{ self.facts.shared_dir }}\nother: {{ other.facts.shared_dir }}",
            )],
            Os::Linux,
        )
        .unwrap();
        assert_eq!(plan.fact("other", SHARED_DIR_FACT), Some("/airlock/other/shared"));
    }

    #[test]
    fn a_step_may_reference_any_participants_shared_dir() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, macos, windows]")
            .replace("participants:", "participants:\n  other: {type: every-kind-client}")
            .replace(minimal_procedure!(), "procedure: [{step: probe, on: subject, path: '{{ subject.facts.shared_dir }}/{{ other.facts.shared_dir }}'}]");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = compile(&yaml, os).unwrap().unwrap();
            let CaseSchema::Participants { procedure, .. } = plan.case().schema() else {
                panic!("participant case")
            };
            let ProcedureNode::Step(step) = &procedure[0] else {
                panic!("single step")
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("subject".into()),
                    Need::Fact("subject".into(), SHARED_DIR_FACT.into()),
                    Need::Fact("other".into(), SHARED_DIR_FACT.into())
                ]
            );
        }
    }

    #[test]
    fn a_connected_socket_outside_the_servers_shared_dir_fails_naming_both_participants() {
        for (endpoint, kind) in [("stream", "unix-stream"), ("datagram", "unix-datagram")] {
            let yaml = socket_case("{}").replace("server.stream", &format!("server.{endpoint}"));
            assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(), format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' connects 'target' to 'server.{endpoint}', a {kind} socket that participant 'server' of type 'every-kind-server' as linux-container on linux serves at '/run/every/{endpoint}.sock', outside its shared_dir '/airlock/server/shared'; consumed sockets must lie under their server's shared_dir"
            ));
        }
    }

    #[test]
    fn a_socket_referenced_by_another_participant_outside_its_shared_dir_fails() {
        let yaml = socket_case("{}").replace(
            "connect: {target: server.stream}",
            "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
        );
        let expected = "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participant 'client' of type 'every-kind-client' as linux-container on linux environment variable 'SOCKET' '{{ server.endpoints.stream.path }}' references endpoint 'stream' of participant 'server', a unix-stream socket served at '/run/every/stream.sock', outside its shared_dir '/airlock/server/shared'; consumed sockets must lie under their server's shared_dir";
        assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(), expected);
        let yaml = format!("timeout: 1s\nsubject_os: [linux]\nparticipants:\n  subject: {{type: file-server, subject: true, config: {FILE_CONFIG}}}\n  server: {{type: file-server}}\n{}", minimal_procedure!());
        let expected = expected
            .replace(
                "'client' of type 'every-kind-client'",
                "'subject' of type 'file-server'",
            )
            .replace("environment variable 'SOCKET'", "configuration file 'settings.yaml'");
        assert_eq!(
            compile_files(
                &yaml,
                &[("source.yaml", "{{ server.endpoints.stream.path }}")],
                Os::Linux
            )
            .err()
            .unwrap(),
            expected
        );
    }

    #[test]
    fn a_participant_may_reference_its_own_socket_anywhere() {
        for participant in ["self", "subject"] {
            let yaml = PARTICIPANT_FIELDS.replace("type: server", &format!("type: every-kind-server, config: {{env: {{SOCKET_DIR: '/dev/null/{}', COPY: '{{{{ {participant}.endpoints.stream.path }}}}'}}}}", "x".repeat(108)));
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_eq!(
                plan.effective_config("subject").unwrap().env["COPY"],
                format!("{{{{ {participant}.endpoints.stream.path }}}}")
            );
        }
    }

    #[test]
    fn an_unconsumed_socket_may_lie_anywhere() {
        let yaml = PARTICIPANT_FIELDS
            .replace(
                "type: server",
                "type: every-kind-server, config: {env: {SOCKET_DIR: '/dev/null'}}",
            )
            .replace(
                "participants:",
                "participants:\n  other: {type: every-kind-client, connect: {target: subject.tcp}}",
            );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(plan.effective_config("subject").unwrap().env["SOCKET_DIR"], "/dev/null");
        assert_eq!(
            plan.effective_config("other").unwrap().env["TARGET"],
            "tcp://subject:1001"
        );
    }

    #[test]
    fn a_consumed_socket_path_over_the_os_limit_fails_and_one_at_the_limit_passes() {
        for (os, dir, limit) in [
            (Os::MacOs, "/tmp/panoramic-testrun0/0000/server/shared", 103),
            (Os::Linux, "/airlock/server/shared", 107),
        ] {
            for reference in [false, true] {
                for length in [limit, limit + 1] {
                    let path = format!("{dir}/{}", "x".repeat(length - dir.len() - 1));
                    let mut yaml = socket_case(&format!("{{env: {{SOCKET_PATH: '{path}'}}}}"))
                        .replace("type: every-kind-server", "type: file-server")
                        .replace("[linux]", &format!("[{}]", os.name()));
                    if reference {
                        yaml = yaml.replace(
                            "connect: {target: server.stream}",
                            "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
                        );
                    }
                    let result = compile(&yaml, os);
                    if length == limit {
                        let plan = result.unwrap().unwrap();
                        assert_eq!(plan.effective_config("server").unwrap().env["SOCKET_PATH"], path);
                    } else {
                        let message = result.err().unwrap().to_string();
                        assert!(
                            message.ends_with(&format!(
                                "at '{path}' ({length} bytes); a socket path on {} holds at most {limit} bytes",
                                os.name()
                            )),
                            "{message}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn socket_location_errors_fail_on_an_unselected_subject_os() {
        let yaml = socket_case(
            "{env: {SOCKET_DIR: '{{ self.facts.shared_dir }}'}, os: {macos: {env: {SOCKET_DIR: /elsewhere}}}}",
        )
        .replace("[linux]", "[linux, macos]");
        for reference in [false, true] {
            let yaml = if reference {
                yaml.replace(
                    "connect: {target: server.stream}",
                    "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
                )
            } else {
                yaml.clone()
            };
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let mut planner = planner();
                for _ in 0..3 {
                    planner.scopes.allocate().unwrap();
                }
                let message = compile_with(&mut planner, &yaml, os).err().unwrap().to_string();
                let index = if os == Os::Windows { 0 } else { 3 };
                assert!(message.contains("on subject OS 'macos'"), "{message}");
                assert!(
                    message.contains(&format!(
                        "outside its shared_dir '/tmp/panoramic-testrun0/{index:04}/server/shared'"
                    )),
                    "{message}"
                );
                assert_eq!(
                    planner.scopes.allocate().unwrap().index,
                    if os == Os::Windows { 3 } else { 4 }
                );
            }
        }
    }

    #[test]
    fn socket_length_errors_fail_on_an_unselected_subject_os() {
        let path = format!(
            "{{{{ self.facts.shared_dir }}}}/{}",
            "x".repeat(104 - "/tmp/panoramic-testrun0/0000/server/shared/".len())
        );
        let yaml = socket_case(&format!("{{env: {{SOCKET_PATH: '{path}'}}}}"))
            .replace("type: every-kind-server", "type: file-server")
            .replace("[linux]", "[linux, macos]");
        for reference in [false, true] {
            let yaml = if reference {
                yaml.replace(
                    "connect: {target: server.stream}",
                    "config: {env: {SOCKET: '{{ server.endpoints.stream.path }}'}}",
                )
            } else {
                yaml.clone()
            };
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                let message = compile(&yaml, os).err().unwrap().to_string();
                assert!(message.contains("on subject OS 'macos'"), "{message}");
                assert!(
                    message.ends_with("(104 bytes); a socket path on macos holds at most 103 bytes"),
                    "{message}"
                );
            }
        }
    }
}
