//! Computes create and start orders from connections and late-fact references, rejecting cycles.

use std::collections::BTreeMap;

use super::endpoints::ParticipantEndpoints;
use crate::{
    backend::{late_facts, LateFactTiming},
    case::{
        config_references::{walk_config_strings, ConfigField, FileTemplates},
        template::{Reference, Template},
        CaseFile,
    },
    layout::{describe_participant, Layout},
};

/// Dependency levels with predecessors in earlier levels and names sorted within each level.
pub(super) struct DependencyOrder {
    pub(super) create: Vec<Vec<String>>,
    pub(super) start: Vec<Vec<String>>,
}

/// Builds create and start orders for one OS from validated configuration.
///
/// A participant is created after each other participant whose late fact its `env`, `params` or template
/// files reference, when that fact becomes available at creation. It starts after participants it connects
/// to and those whose late facts, available only after start, its pod template files reference. Own late facts
/// are filled in by init hooks or late files; fixed late facts are known before creation. Addresses use names
/// and ports allocated before creation, so `connect` declares their startup dependencies.
pub(super) fn dependency_order(
    file: &CaseFile, templates: &FileTemplates, layout: &Layout, endpoints: &BTreeMap<String, ParticipantEndpoints>,
) -> Result<DependencyOrder, String> {
    let mut create = DependencyGraph::new(endpoints.keys());
    let mut start = DependencyGraph::new(endpoints.keys());
    for (name, participant) in endpoints {
        let entry = &file.participants[name];
        let realization = layout.get(name).expect("loaded participants have a layout");
        let holder = describe_participant(name, &entry.participant_type, realization);
        for (connection, target) in &entry.connect {
            start.add(
                name,
                &target.participant,
                format!(
                    "participant '{name}' connects '{connection}' to '{}.{}'",
                    target.participant, target.endpoint,
                ),
            );
        }
        let mut collect = |field: ConfigField<'_>, template: &Template| {
            for (written, reference) in template.written_references() {
                let Reference::Late { participant, fact } = reference else {
                    continue;
                };
                let target = participant.name(name);
                if target == name {
                    continue;
                }
                let target_realization = layout.get(target).expect("referenced participants have a layout");
                let (_, timing) = late_facts(target_realization.kind)
                    .iter()
                    .find(|(provided, _)| provided == fact)
                    .expect("configuration late facts were validated");
                let graph = match timing {
                    LateFactTiming::AfterCreate => &mut create,
                    LateFactTiming::AfterStart => &mut start,
                    LateFactTiming::Fixed(_) => continue,
                };
                graph.add(
                    name,
                    target,
                    format!(
                        "{holder} {field} '{written}' references late fact '{}' of participant '{target}'",
                        fact.name(),
                    ),
                );
            }
        };
        walk_config_strings(&participant.values.env, &participant.values.params, |field, text| {
            let template: Template = text
                .parse()
                .expect("substituted configuration retains valid references");
            collect(field, &template);
            Ok(())
        })?;
        for (file_name, source) in participant.values.files.iter().filter(|(_, source)| source.template) {
            collect(ConfigField::File(file_name), templates.get(&source.from));
        }
    }
    Ok(DependencyOrder {
        create: create.levels("created")?,
        start: start.levels("started")?,
    })
}

struct DependencyGraph {
    predecessors: BTreeMap<String, BTreeMap<String, String>>,
}

impl DependencyGraph {
    fn new<'a>(participants: impl Iterator<Item = &'a String>) -> Self {
        Self {
            predecessors: participants.map(|name| (name.clone(), BTreeMap::new())).collect(),
        }
    }

    fn add(&mut self, participant: &str, predecessor: &str, reason: String) {
        self.predecessors
            .get_mut(participant)
            .expect("every participant has a graph node")
            .entry(predecessor.to_string())
            .or_insert(reason);
    }

    fn levels(mut self, action: &str) -> Result<Vec<Vec<String>>, String> {
        let mut levels = Vec::new();
        while !self.predecessors.is_empty() {
            let level: Vec<_> = self
                .predecessors
                .iter()
                .filter(|(_, predecessors)| predecessors.is_empty())
                .map(|(name, _)| name.clone())
                .collect();
            if level.is_empty() {
                return Err(self.cycle_error(action));
            }
            for name in &level {
                self.predecessors.remove(name);
            }
            for predecessors in self.predecessors.values_mut() {
                for name in &level {
                    predecessors.remove(name);
                }
            }
            levels.push(level);
        }
        Ok(levels)
    }

    fn cycle_error(&self, action: &str) -> String {
        let mut path = Vec::new();
        let mut current = self.predecessors.keys().next().expect("a cycle has participants");
        let mut cycle = loop {
            if let Some(index) = path.iter().position(|name| *name == current) {
                break path[index..].to_vec();
            }
            path.push(current);
            current = self.predecessors[current]
                .keys()
                .next()
                .expect("every remaining participant has a predecessor");
        };
        // Predecessor traversal runs against edge direction; report the cycle from its smallest name.
        cycle.reverse();
        let first = cycle.iter().enumerate().min_by_key(|(_, name)| *name).unwrap().0;
        cycle.rotate_left(first);
        let reasons = cycle
            .iter()
            .zip(cycle.iter().cycle().skip(1))
            .map(|(from, to)| {
                let reason = &self.predecessors[*to][*from];
                format!("'{from}' must be {action} before '{to}' because {reason}")
            })
            .collect::<Vec<_>>();
        format!("participants can't be {action} in any order: {}", reasons.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        case::fixtures::{minimal_procedure, FixtureConfig},
        layout::Os,
        plan::{
            fixtures::{compile, compile_files},
            CasePlan,
        },
    };

    const SERVERS: &str = concat!(
        r#"timeout: 1s
subject_os: [linux]
participants:
  subject: {type: server, subject: true}
  fixture: {type: server}
"#,
        minimal_procedure!(),
        "\n"
    );

    const FILE_CASE: &str = concat!(
        r#"timeout: 1s
subject_os: [linux]
participants:
  subject:
    type: file-server
    subject: true
    config: {files: {settings.yaml: {from: source.yaml, template: true}}}
  fixture: {type: file-server}
"#,
        minimal_procedure!(),
        "\n"
    );

    #[track_caller]
    fn assert_orders(plan: &CasePlan<FixtureConfig>, create: &[&[&str]], start: &[&[&str]]) {
        for (label, actual, expected) in [
            ("create", plan.create_order(), create),
            ("start", plan.start_order(), start),
        ] {
            let expected: Vec<Vec<String>> = expected
                .iter()
                .map(|level| level.iter().map(|name| name.to_string()).collect())
                .collect();
            assert_eq!(actual.unwrap(), expected, "{label} order");
        }
    }

    #[test]
    fn every_admitted_late_reference_adds_the_edge_its_timing_implies() {
        use crate::layout::RealizationKind::{HostProcessGroup, KubernetesPod, LinuxContainer, WindowsContainer};

        #[derive(Clone, Copy, Debug)]
        enum Outcome {
            Rejected,
            NoEdge,
            Create,
            Start,
        }
        use Outcome::{Create, NoEdge, Rejected, Start};

        // Rows: env, params, template file. Columns: ip, container_id, pod_uid.
        let cases = [
            (
                LinuxContainer,
                None,
                [
                    [NoEdge, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                LinuxContainer,
                Some(LinuxContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                ],
            ),
            (
                LinuxContainer,
                Some(WindowsContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                ],
            ),
            (
                LinuxContainer,
                Some(HostProcessGroup),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                LinuxContainer,
                Some(KubernetesPod),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                WindowsContainer,
                None,
                [
                    [NoEdge, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                WindowsContainer,
                Some(LinuxContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                ],
            ),
            (
                WindowsContainer,
                Some(WindowsContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                ],
            ),
            (
                WindowsContainer,
                Some(HostProcessGroup),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                WindowsContainer,
                Some(KubernetesPod),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                HostProcessGroup,
                None,
                [
                    [NoEdge, Rejected, Rejected],
                    [NoEdge, Rejected, Rejected],
                    [NoEdge, Rejected, Rejected],
                ],
            ),
            (
                HostProcessGroup,
                Some(LinuxContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                ],
            ),
            (
                HostProcessGroup,
                Some(WindowsContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                ],
            ),
            (
                HostProcessGroup,
                Some(HostProcessGroup),
                [
                    [NoEdge, Rejected, Rejected],
                    [NoEdge, Rejected, Rejected],
                    [NoEdge, Rejected, Rejected],
                ],
            ),
            (
                HostProcessGroup,
                Some(KubernetesPod),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                KubernetesPod,
                None,
                [
                    [NoEdge, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [NoEdge, NoEdge, NoEdge],
                ],
            ),
            (
                KubernetesPod,
                Some(LinuxContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Start, Create, Rejected],
                ],
            ),
            (
                KubernetesPod,
                Some(WindowsContainer),
                [
                    [Rejected, Create, Rejected],
                    [Rejected, Create, Rejected],
                    [Start, Create, Rejected],
                ],
            ),
            (
                KubernetesPod,
                Some(HostProcessGroup),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                ],
            ),
            (
                KubernetesPod,
                Some(KubernetesPod),
                [
                    [Rejected, Rejected, Rejected],
                    [Rejected, Rejected, Rejected],
                    [Start, Start, Start],
                ],
            ),
        ];
        let flat = [vec!["holder".to_string(), "target".to_string()]];
        let ordered = [vec!["target".to_string()], vec!["holder".to_string()]];
        for (holder_kind, target, outcomes) in cases {
            let reference_name = if target.is_some() { "target" } else { "self" };
            let target_kind = target.unwrap_or(holder_kind);
            let [holder_os, target_os] = [holder_kind, target_kind].map(|kind| {
                if kind == WindowsContainer {
                    Os::Windows
                } else {
                    Os::Linux
                }
            });
            for (field, outcomes) in ["env", "params", "file"].into_iter().zip(outcomes) {
                for (fact, expected) in ["ip", "container_id", "pod_uid"].into_iter().zip(outcomes) {
                    let context = format!("holder {holder_kind:?}, target {target:?}, {field}, {fact}");
                    let reference = format!("{{{{{reference_name}.late.{fact}}}}}");
                    let config = match field {
                        "env" => format!("{{env: {{KEY: '{reference}'}}}}"),
                        "params" => format!("{{params: {{mode: '{reference}'}}}}"),
                        "file" => "{files: {settings.yaml: {from: source.yaml, template: true}}}".to_string(),
                        _ => unreachable!(),
                    };
                    let yaml = format!(
                        "timeout: 1s\nsubject_os: [{}]\nparticipants:\n  holder: {{type: file-server, subject: true, realization: {{kind: {}}}, config: {config}}}\n  target: {{type: file-server, os: {}, realization: {{kind: {}}}, config: {{env: {{PORT_OFFSET: '100'}}}}}}\n{}\n",
                        holder_os.name(),
                        holder_kind.name(),
                        target_os.name(),
                        target_kind.name(),
                        minimal_procedure!(),
                    );
                    match (
                        expected,
                        compile_files(&yaml, &[("source.yaml", &reference)], holder_os),
                    ) {
                        (Rejected, Err(error)) => assert!(
                            error.contains("references") && error.contains("late fact"),
                            "{context}: {error}",
                        ),
                        (Rejected, Ok(_)) => panic!("{context}: expected rejection"),
                        (_, Err(error)) => panic!("{context}: {error}"),
                        (expected, Ok(plan)) => {
                            let (create, start) = match expected {
                                NoEdge => (flat.as_slice(), flat.as_slice()),
                                Create => (ordered.as_slice(), flat.as_slice()),
                                Start => (flat.as_slice(), ordered.as_slice()),
                                Rejected => unreachable!(),
                            };
                            assert_eq!(plan.create_order(), Some(create), "{context}: create order");
                            assert_eq!(plan.start_order(), Some(start), "{context}: start order");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn env_and_nested_params_needing_another_containers_id_order_creation_only() {
        for config in [
            "{env: {KEY: '{{fixture.late.container_id}}'}}",
            "{params: {mode: [false, {nested: ['{{fixture.late.container_id}}']}]}}",
        ] {
            let yaml = SERVERS
                .replace("[linux]", "[linux, windows]")
                .replace("subject: true", &format!("subject: true, config: {config}"));
            for os in [Os::Linux, Os::Windows] {
                let plan = compile(&yaml, os).unwrap().unwrap();
                assert_orders(&plan, &[&["fixture"], &["subject"]], &[&["fixture", "subject"]]);
            }
        }
    }

    #[test]
    fn template_files_needing_another_containers_id_order_creation_even_for_pods() {
        for kind in ["linux-container", "kubernetes-pod"] {
            let yaml = FILE_CASE.replace(
                "subject: true",
                &format!("subject: true\n    realization: {{kind: {kind}}}"),
            );
            for target in [
                "type: file-server, realization: {kind: linux-container}",
                "type: file-server, os: windows",
            ] {
                let yaml = yaml.replace("fixture: {type: file-server}", &format!("fixture: {{{target}}}"));
                let plan =
                    compile_files(&yaml, &[("source.yaml", "{{fixture.late.container_id}}")], Os::Linux).unwrap();
                assert_orders(&plan, &[&["fixture"], &["subject"]], &[&["fixture", "subject"]]);
            }
        }
    }

    #[test]
    fn connections_order_startup_but_not_creation() {
        let yaml = SERVERS.replace(
            "type: server, subject: true",
            "type: client, subject: true, connect: {upstream: fixture.api}",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture"], &["subject"]]);
    }

    #[test]
    fn a_fixture_can_be_created_after_its_subject_but_started_before_it() {
        let yaml = SERVERS
            .replace(
                "type: server, subject: true",
                "type: client, subject: true, connect: {upstream: fixture.api}",
            )
            .replace(
                "fixture: {type: server}",
                "fixture: {type: server, config: {params: {mode: '{{subject.late.container_id}}'}}}",
            );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_orders(&plan, &[&["subject"], &["fixture"]], &[&["fixture"], &["subject"]]);
    }

    #[test]
    fn a_pods_own_late_file_references_add_no_dependencies() {
        let yaml = FILE_CASE.replace(
            "subject: true",
            "subject: true\n    realization: {kind: kubernetes-pod}",
        );
        for name in ["self", "subject"] {
            for fact in ["ip", "container_id", "pod_uid"] {
                let text = format!("{{{{{name}.late.{fact}}}}}");
                let plan = compile_files(&yaml, &[("source.yaml", &text)], Os::Linux).unwrap();
                assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
            }
        }
    }

    #[test]
    fn a_pods_file_needing_another_participants_after_start_fact_orders_startup_only() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [linux]
participants:
  load-a:
    type: file-server
    subject: true
    realization: {kind: kubernetes-pod}
    config: {files: {settings.yaml: {from: own.yaml, template: true}}}
  load-b:
    type: file-server
    realization: {kind: kubernetes-pod}
    config: {files: {settings.yaml: {from: other.yaml, template: true}}}
"#,
            minimal_procedure!(),
            "\n"
        );
        for fact in ["ip", "container_id", "pod_uid"] {
            let text = format!("{{{{load-a.late.{fact}}}}}");
            let plan = compile_files(
                yaml,
                &[("own.yaml", "{{self.late.pod_uid}}"), ("other.yaml", &text)],
                Os::Linux,
            )
            .unwrap();
            assert_orders(&plan, &[&["load-a", "load-b"]], &[&["load-a"], &["load-b"]]);
        }
    }

    #[test]
    fn a_pods_file_and_a_connection_can_form_a_start_cycle() {
        let yaml = FILE_CASE
            .replace(
                "subject: true",
                "subject: true\n    realization: {kind: kubernetes-pod}",
            )
            .replace(
                "fixture: {type: file-server}",
                "fixture: {type: file-server, connect: {target: subject.tcp}}",
            );
        let message = compile_files(&yaml, &[("source.yaml", "{{fixture.late.pod_uid}}")], Os::Linux)
            .err()
            .unwrap();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participants can't be started in any order: 'fixture' must be started before 'subject' because participant 'subject' of type 'file-server' as kubernetes-pod on linux configuration file 'settings.yaml' '{{fixture.late.pod_uid}}' references late fact 'pod_uid' of participant 'fixture'; 'subject' must be started before 'fixture' because participant 'fixture' connects 'target' to 'subject.tcp'");
    }

    #[test]
    fn fixed_ip_and_fact_references_add_no_dependencies() {
        let yaml = FILE_CASE.replace("[linux]", "[macos]").replace(
            "config: {files:",
            "config: {env: {PORT_OFFSET: '100', KEY: '{{fixture.late.ip}}'}, params: {mode: '{{fixture.late.ip}}'}, files:",
        );
        let plan = compile_files(&yaml, &[("source.yaml", "{{fixture.late.ip}}")], Os::MacOs).unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
        let yaml = yaml
            .replace("[macos]", "[linux]")
            .replace("fixture.late.ip", "fixture.facts.file");
        let plan = compile_files(&yaml, &[("source.yaml", "{{fixture.facts.file}}")], Os::Linux).unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
    }

    #[test]
    fn own_late_facts_substituted_in_env_add_no_dependencies() {
        for name in ["self", "subject"] {
            let yaml = SERVERS.replace(
                "subject: true",
                &format!("subject: true, config: {{env: {{KEY: '{{{{{name}.late.ip}}}}'}}}}"),
            );
            let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
            assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
        }
    }

    #[test]
    fn mutual_endpoint_references_add_no_dependencies() {
        let yaml = SERVERS
            .replace(
                "subject: true",
                "subject: true, config: {env: {KEY: '{{fixture.endpoints.api.url}}'}}",
            )
            .replace(
                "fixture: {type: server}",
                "fixture: {type: server, config: {params: {mode: '{{subject.endpoints.api.host}}'}}}",
            );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
        let yaml = FILE_CASE.replace(
            "fixture: {type: file-server}",
            "fixture: {type: file-server, config: {env: {KEY: '{{subject.endpoints.tcp.url}}'}}}",
        );
        let plan = compile_files(&yaml, &[("source.yaml", "{{fixture.endpoints.tcp.url}}")], Os::Linux).unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
    }

    #[test]
    fn connection_references_add_nothing_beyond_the_connect_edge() {
        let yaml = FILE_CASE.replace("subject: true", "subject: true\n    connect: {target: fixture.tcp}").replace(
            "config: {files:",
            "config: {env: {KEY: '{{self.connections.target.url}}'}, params: {mode: '{{self.connections.target.host}}'}, files:",
        );
        let plan = compile_files(&yaml, &[("source.yaml", "{{self.connections.target.port}}")], Os::Linux).unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture"], &["subject"]]);
    }

    #[test]
    fn step_late_fact_references_add_no_dependencies() {
        let yaml = SERVERS.replace(
            minimal_procedure!(),
            "procedure: [{step: probe, on: subject, path: '{{fixture.late.container_id}}/{{fixture.late.ip}}'}]",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
    }

    #[test]
    fn levels_are_sorted_and_wait_for_every_predecessor() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [linux]
participants:
  subject:
    type: file-server
    subject: true
    config: {files: {settings.yaml: {from: both.yaml, template: true}}}
  spare: {type: file-server}
  right: {type: file-server, config: {files: {settings.yaml: {from: root.yaml, template: true}}}}
  root: {type: file-server}
  left: {type: file-server, config: {files: {settings.yaml: {from: root.yaml, template: true}}}}
"#,
            minimal_procedure!(),
            "\n"
        );
        let all: &[&[&str]] = &[&["left", "right", "root", "spare", "subject"]];
        let layered: &[&[&str]] = &[&["root", "spare"], &["left", "right"], &["subject"]];
        for pod in [false, true] {
            let (yaml, fact, create, start) = if pod {
                (
                    yaml.replace(
                        "subject: true",
                        "subject: true\n    realization: {kind: kubernetes-pod}",
                    ),
                    "pod_uid",
                    all,
                    layered,
                )
            } else {
                (yaml.to_string(), "container_id", layered, all)
            };
            let both = format!("{{{{left.late.{fact}}}}} {{{{right.late.{fact}}}}} {{{{root.late.{fact}}}}}");
            let root = format!("{{{{root.late.{fact}}}}}");
            let plan = compile_files(&yaml, &[("both.yaml", &both), ("root.yaml", &root)], Os::Linux).unwrap();
            assert_orders(&plan, create, start);
        }
    }

    #[test]
    fn a_create_cycle_names_each_reference_in_edge_direction() {
        let yaml = SERVERS
            .replace(
                "subject: true",
                "subject: true, config: {env: {KEY: '{{ fixture.late.container_id }}'}}",
            )
            .replace(
                "fixture: {type: server}",
                "fixture: {type: server, config: {params: {mode: '{{subject.late.container_id}}'}}}",
            );
        assert_eq!(compile(&yaml, Os::Linux).err().unwrap().to_string(),
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participants can't be created in any order: 'fixture' must be created before 'subject' because participant 'subject' of type 'server' as linux-container on linux environment variable 'KEY' '{{ fixture.late.container_id }}' references late fact 'container_id' of participant 'fixture'; 'subject' must be created before 'fixture' because participant 'fixture' of type 'server' as linux-container on linux parameter 'mode' '{{subject.late.container_id}}' references late fact 'container_id' of participant 'subject'");
    }

    #[test]
    fn a_start_cycle_skips_acyclic_edges_and_starts_at_its_smallest_name_in_edge_direction() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [linux]
participants:
  a: {type: file-server, subject: true, connect: {target: b.tcp}}
  b: {type: file-server, connect: {target: c.tcp}, config: {files: {settings.yaml: {from: e.yaml, template: true}}}}
  c: {type: file-server, connect: {target: d.tcp}}
  d: {type: file-server, connect: {target: b.tcp}, config: {files: {settings.yaml: {from: source.yaml, template: true}}}}
  e: {type: file-server, connect: {target: b.tcp}}
  z: {type: file-server}
"#,
            minimal_procedure!(),
            "\n"
        )
        .replace("type: file-server", "type: file-server, realization: {kind: kubernetes-pod}");
        let message = compile_files(
            &yaml,
            &[("source.yaml", "{{z.late.pod_uid}}"), ("e.yaml", "{{e.late.pod_uid}}")],
            Os::Linux,
        )
        .err()
        .unwrap();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participants can't be started in any order: 'b' must be started before 'd' because participant 'd' connects 'target' to 'b.tcp'; 'd' must be started before 'c' because participant 'c' connects 'target' to 'd.tcp'; 'c' must be started before 'b' because participant 'b' connects 'target' to 'c.tcp'");
    }

    #[test]
    fn duplicate_start_edges_report_connections_before_references() {
        let yaml = concat!(
            r#"timeout: 1s
subject_os: [linux]
participants:
  subject: {type: file-server, subject: true, connect: {target: fixture.tcp}, config: {files: {settings.yaml: {from: source.yaml, template: true}}}}
  fixture: {type: file-server, connect: {target: subject.tcp}}
"#,
            minimal_procedure!(),
            "\n"
        )
        .replace("type: file-server", "type: file-server, realization: {kind: kubernetes-pod}");
        let message = compile_files(
            &yaml,
            &[("source.yaml", "{{fixture.late.pod_uid}} {{fixture.late.ip}}")],
            Os::Linux,
        )
        .err()
        .unwrap();
        assert_eq!(message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participants can't be started in any order: 'fixture' must be started before 'subject' because participant 'subject' connects 'target' to 'fixture.tcp'; 'subject' must be started before 'fixture' because participant 'fixture' connects 'target' to 'subject.tcp'");
    }

    #[test]
    fn duplicate_reference_edges_keep_the_first_in_field_and_reference_order() {
        let mut yaml = concat!(
            r#"timeout: 1s
subject_os: [linux]
participants:
  subject:
    type: file-server
    subject: true
    config:
      env: {Z: '{{ fixture.late.container_id}}', A: '{{fixture.late.container_id}}'}
      params: {mode: {z: '{{  fixture.late.container_id}}', a: ['{{fixture.late.container_id }}', '{{fixture.late.container_id  }}']}}
      files:
        z.yaml: {from: z.yaml, template: true}
        settings.yaml: {from: source.yaml, template: true}
  fixture: {type: file-server, config: {params: {mode: '{{subject.late.container_id}}'}}}
"#,
            minimal_procedure!(),
            "\n"
        )
        .to_string();
        let mut sources = [
            (
                "source.yaml",
                "{{ fixture.late.container_id }} {{  fixture.late.container_id  }}".to_string(),
            ),
            ("z.yaml", "{{   fixture.late.container_id   }}".to_string()),
        ];
        for (field, written) in [
            ("environment variable 'A'", "{{fixture.late.container_id}}"),
            ("environment variable 'Z'", "{{ fixture.late.container_id}}"),
            ("parameter 'mode'", "{{fixture.late.container_id }}"),
            ("parameter 'mode'", "{{fixture.late.container_id  }}"),
            ("parameter 'mode'", "{{  fixture.late.container_id}}"),
            ("configuration file 'settings.yaml'", "{{ fixture.late.container_id }}"),
            (
                "configuration file 'settings.yaml'",
                "{{  fixture.late.container_id  }}",
            ),
            ("configuration file 'z.yaml'", "{{   fixture.late.container_id   }}"),
        ] {
            let sources_ref: Vec<_> = sources.iter().map(|(name, text)| (*name, text.as_str())).collect();
            let message = compile_files(&yaml, &sources_ref, Os::Linux).err().unwrap();
            assert_eq!(message, format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', participants can't be created in any order: 'fixture' must be created before 'subject' because participant 'subject' of type 'file-server' as linux-container on linux {field} '{written}' references late fact 'container_id' of participant 'fixture'; 'subject' must be created before 'fixture' because participant 'fixture' of type 'file-server' as linux-container on linux parameter 'mode' '{{{{subject.late.container_id}}}}' references late fact 'container_id' of participant 'subject'"
            ));
            yaml = yaml.replace(written, "literal");
            for (_, text) in &mut sources {
                *text = text.replace(written, "literal");
            }
        }
        let sources_ref: Vec<_> = sources.iter().map(|(name, text)| (*name, text.as_str())).collect();
        let plan = compile_files(&yaml, &sources_ref, Os::Linux).unwrap();
        assert_orders(&plan, &[&["subject"], &["fixture"]], &[&["fixture", "subject"]]);
    }

    #[test]
    fn only_template_files_in_effect_for_the_participants_os_add_edges() {
        let yaml = FILE_CASE.replace("[linux]", "[linux, windows]").replace(
            "config: {files: {settings.yaml: {from: source.yaml, template: true}}}",
            "config: {files: {settings.yaml: plain.yaml}, os: {windows: {files: {settings.yaml: {from: source.yaml, template: true}}}}}",
        );
        let sources = [
            ("source.yaml", "{{fixture.late.container_id}}"),
            ("plain.yaml", "{{fixture.late.container_id}}"),
        ];
        let plan = compile_files(&yaml, &sources, Os::Linux).unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
        let plan = compile_files(&yaml, &sources, Os::Windows).unwrap();
        assert_orders(&plan, &[&["fixture"], &["subject"]], &[&["fixture", "subject"]]);

        let yaml = yaml.replace("subject: true", "os: windows").replace(
            "fixture: {type: file-server}",
            "fixture: {type: file-server, subject: true}",
        );
        let plan = compile_files(&yaml, &sources, Os::Linux).unwrap();
        assert_orders(&plan, &[&["fixture"], &["subject"]], &[&["fixture", "subject"]]);
    }

    #[test]
    fn an_overlay_adds_edges_only_on_its_os_and_a_cycle_fails_every_selection() {
        let yaml = SERVERS.replace("[linux]", "[linux, windows]").replace(
            "subject: true",
            "subject: true, config: {os: {windows: {env: {KEY: '{{fixture.late.container_id}}'}}}}",
        );
        let plan = compile(&yaml, Os::Linux).unwrap().unwrap();
        assert_orders(&plan, &[&["fixture", "subject"]], &[&["fixture", "subject"]]);
        let plan = compile(&yaml, Os::Windows).unwrap().unwrap();
        assert_orders(&plan, &[&["fixture"], &["subject"]], &[&["fixture", "subject"]]);
        let yaml = yaml.replace(
            "fixture: {type: server}",
            "fixture: {type: server, config: {params: {mode: '{{subject.late.container_id}}'}}}",
        );
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(compile(&yaml, os).err().unwrap().to_string(),
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', participants can't be created in any order: 'fixture' must be created before 'subject' because participant 'subject' of type 'server' as windows-container on windows environment variable 'KEY' '{{fixture.late.container_id}}' references late fact 'container_id' of participant 'fixture'; 'subject' must be created before 'fixture' because participant 'fixture' of type 'server' as windows-container on windows parameter 'mode' '{{subject.late.container_id}}' references late fact 'container_id' of participant 'subject'");
        }
    }
}
