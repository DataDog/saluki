//! Checks ADP's startup defaults against the Agent schema's platform rules.
//!
//! Bootstrap replay loads each test case's original YAML and environment into ADP, before the Agent
//! supplies configuration. It compares ADP's values with saved Agent results (the corpus), which
//! may have been recorded on another operating system.
//!
//! A declaration is a setting's `platform_default` mapping in the schema. Like the Agent's
//! `getPlatformDefault`, we use a declared Fargate entry when on ECS Fargate, then a container entry
//! when containerized, then the operating system, then `other`, then the type's zero value.
//! A platform context consists of the operating system and container/Fargate flags. Expected values
//! never come from ADP's default-selection code; only schema loading is shared.
//!
//! A check is eligible for a host-specific expectation only if:
//!
//! - the schema type, Agent getter and ADP field type can be compared without conversion;
//! - the original inputs cannot set the key (ambiguous inputs do not qualify);
//! - the Agent reports a default as the source;
//! - the schema's value for the recorded context equals both the value the Agent sent and the
//!   getter result. This guards against Agent startup adjustments that still report `default`.
//!
//! For an eligible check, we change only the context's operating system to the replay host's.
//! If the selected value differs from the recording, we generate an `Intentional` expectation:
//! an exact required result for an expected platform difference, not an allowed bug. Even matching
//! the recording fails when the host should use another value. All other checks keep their ordinary
//! comparisons and handwritten expectations; no check is skipped.
//!
//! ADP reads container/Fargate flags from the test process's environment, but expected values use
//! the recorded case's flags. If these differ, an environment-sensitive default may fail the test.
//! Do not hide that failure by changing the expected flags to match the process.

use std::collections::BTreeMap;
use std::fmt;

use datadog_agent_config_corpus::{Case, Corpus, Getter, GetterResult, Outcome, Source};
use datadog_agent_config_overlay_model::{load_resolved_schema, Files};
use serde_json::{Map, Value};

use super::compare::LeafKind;
use super::expectations::{render_result, Adp, Basis, Cause, CheckKey, Expectation, Status, Tier};

const PLATFORM_DEFAULT: Cause = Cause {
    name: "platform-default",
    why: "the schema's platform default selects another value on this host than on the recording platform",
    issue: None,
};

/// Whether a recorded default justifies a host-specific expected result.
#[derive(Debug)]
pub(crate) struct Assessment {
    /// Why the default does or does not apply, for failure reports.
    pub(crate) note: String,
    /// An exact host result, only when it differs from the recording.
    pub(crate) expectation: Option<Expectation>,
}

/// Schema platform defaults used to check results for one replay operating system.
pub(crate) struct PlatformDefaults {
    declarations: BTreeMap<String, Declaration>,
    host_goos: String,
}

impl PlatformDefaults {
    /// Loads platform rules for this process's operating system.
    pub(crate) fn for_host() -> Self {
        let goos = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        Self::for_goos(goos)
    }

    /// Loads platform rules for `goos` (Go's OS name, such as `darwin`).
    fn for_goos(goos: &str) -> Self {
        Self::from_schema(&vendored_schema(), goos)
    }

    /// Reads platform rules from `schema` for replay on `goos`.
    fn from_schema(schema: &Value, goos: &str) -> Self {
        let mut declarations = BTreeMap::new();
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            collect_declarations(properties, "", &mut declarations);
        }
        Self {
            declarations,
            host_goos: goos.to_string(),
        }
    }

    /// Checks whether the recorded default justifies a host-specific expectation.
    /// Returns `None` outside bootstrap snapshots or when the key has no platform default.
    pub(crate) fn assess(
        &self, corpus: &Corpus, case: &Case, check: &CheckKey, kind: Option<LeafKind>,
    ) -> Option<Assessment> {
        if check.tier != Tier::Bootstrap || check.at != "snapshot" {
            return None;
        }
        let declaration = self.declarations.get(&check.key)?;
        let recorded = Context::recorded(corpus, case);
        let host = Context {
            goos: self.host_goos.clone(),
            ..recorded.clone()
        };
        let (recorded_entry, recorded_value) = match declaration.explain(case, check, kind, &recorded) {
            Ok(explained) => explained,
            Err(why) => {
                return Some(Assessment {
                    note: format!(
                        "the schema declares {declaration}, but it does not explain this check: {why}. The check \
                         compares with the recorded Agent result; a failure is not a platform difference."
                    ),
                    expectation: None,
                })
            }
        };
        let (host_entry, host_value) = declaration.select(&host);
        let selected = format!(
            "the schema declares {declaration}. For the recording ({recorded}) it selects {recorded_value} from \
             {recorded_entry}, as recorded; for this host ({host}) it selects {}",
            host_value.as_ref().map_or_else(
                || format!("{host_entry}, which is not a {}", declaration.ty),
                |v| format!("{v} from {host_entry}")
            ),
        );
        Some(match host_value {
            Some(host_value) if host_value == recorded_value => Assessment {
                note: format!("{selected}, so the check compares with the recorded Agent result."),
                expectation: None,
            },
            Some(host_value) => Assessment {
                expectation: Some(Expectation {
                    check: check.clone(),
                    status: Status::Intentional,
                    adp: Adp::Value(host_value.to_string()),
                    cause: PLATFORM_DEFAULT,
                    at: Basis::PlatformDefault(format!("derived by {}:{} because {selected}.", file!(), line!())),
                }),
                note: format!("{selected}, so ADP must return {host_value}, not the recorded value."),
            },
            None => Assessment {
                note: format!("{selected}. The check compares with the recorded Agent result."),
                expectation: None,
            },
        })
    }
}

/// Loads the checked-in Agent schema, resolving references to other schema files.
fn vendored_schema() -> Value {
    let path = Files::default().datadog_schema;
    load_resolved_schema(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Collects platform-default mappings by lowercase dotted key, including nested sections.
fn collect_declarations(properties: &Map<String, Value>, prefix: &str, out: &mut BTreeMap<String, Declaration>) {
    for (name, node) in properties {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(children) = node.get("properties").and_then(Value::as_object) {
            collect_declarations(children, &key, out);
        }
        let Some(entries) = node.get("platform_default").and_then(Value::as_object) else {
            continue;
        };
        // Conservatively reject the conventional DD_ name even if the schema omits it.
        let mut env_vars = vec![format!("DD_{}", key.to_uppercase().replace('.', "_"))];
        env_vars.extend(
            node.get("env_vars")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string),
        );
        out.insert(
            key.to_lowercase(),
            Declaration {
                ty: node.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                entries: entries.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                env_vars,
                key,
            },
        );
    }
}

/// Operating system and environment flags used to select a default.
#[derive(Clone, Debug)]
struct Context {
    /// Go's `runtime.GOOS` name.
    goos: String,
    /// The Agent's `env.IsContainerized()`.
    containerized: bool,
    /// The Agent's `env.IsECSFargate()`.
    fargate: bool,
}

impl Context {
    /// Uses the recorded OS/container flag and derives Fargate from the case's environment.
    /// The recorder starts each case with only those environment variables.
    fn recorded(corpus: &Corpus, case: &Case) -> Self {
        let env = case.inputs.env.as_ref();
        let var = |name: &str| env.and_then(|env| env.get(name)).map(String::as_str);
        Self {
            goos: corpus.header.goos.clone(),
            containerized: matches!(&case.outcome, Outcome::Started(started) if started.containerized),
            fargate: var("ECS_FARGATE").is_some_and(|v| !v.is_empty())
                || var("AWS_EXECUTION_ENV") == Some("AWS_ECS_FARGATE"),
        }
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let not = |flag: bool| if flag { "" } else { "not " };
        write!(
            f,
            "goos {}, {}containerized, {}on ECS Fargate",
            self.goos,
            not(self.containerized),
            not(self.fargate)
        )
    }
}

/// Schema values we can compare directly with recorded Agent getter results.
#[derive(Clone, Debug, PartialEq)]
enum Declared {
    String(String),
    Integer(i64),
    Boolean(bool),
}

impl Declared {
    /// Accepts the declared type without conversion; a missing entry becomes `""`, `0` or `false`.
    fn of(ty: &str, value: Option<&Value>) -> Option<Declared> {
        Some(match (ty, value) {
            ("string", None) => Declared::String(String::new()),
            ("string", Some(Value::String(s))) => Declared::String(s.clone()),
            ("integer", None) => Declared::Integer(0),
            ("integer", Some(Value::Number(n))) => Declared::Integer(n.as_i64()?),
            ("boolean", None) => Declared::Boolean(false),
            ("boolean", Some(Value::Bool(b))) => Declared::Boolean(*b),
            _ => return None,
        })
    }

    fn is_streamed(&self, streamed: &Value) -> bool {
        match (self, streamed) {
            (Declared::String(a), Value::String(b)) => a == b,
            (Declared::Integer(a), Value::Number(b)) => b.as_i64() == Some(*a),
            (Declared::Boolean(a), Value::Bool(b)) => a == b,
            _ => false,
        }
    }

    fn is_read(&self, result: &GetterResult) -> bool {
        match (self, result) {
            (Declared::String(a), GetterResult::String(b)) => a == b,
            (Declared::Integer(a), GetterResult::Int(b)) => *a == b.value,
            (Declared::Boolean(a), GetterResult::Bool(b)) => a == b,
            _ => false,
        }
    }
}

/// Uses the comparison's ADP formatting so the judge can match the value exactly.
impl fmt::Display for Declared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Declared::String(s) => write!(f, "{s:?}"),
            Declared::Integer(n) => write!(f, "{n}"),
            Declared::Boolean(b) => write!(f, "{b}"),
        }
    }
}

/// One setting's `platform_default` mapping in the schema.
#[derive(Debug)]
struct Declaration {
    /// The dotted key, as the schema spells it.
    key: String,
    /// The schema `type`.
    ty: String,
    /// Platform name to default value.
    entries: BTreeMap<String, Value>,
    /// Schema bindings plus the conventional `DD_` name.
    env_vars: Vec<String>,
}

impl Declaration {
    /// Selects by Agent precedence; returns no value for unsupported types or mismatched values.
    fn select(&self, context: &Context) -> (String, Option<Declared>) {
        let candidates = [
            context.fargate.then_some("fargate"),
            context.containerized.then_some("container"),
            Some(context.goos.as_str()),
            Some("other"),
        ];
        match candidates.into_iter().flatten().find(|c| self.entries.contains_key(*c)) {
            Some(entry) => (format!("`{entry}`"), Declared::of(&self.ty, self.entries.get(entry))),
            None => ("no entry (the zero value)".to_string(), Declared::of(&self.ty, None)),
        }
    }

    /// Verifies input absence, a default source and agreement with the schema.
    fn explain(
        &self, case: &Case, check: &CheckKey, kind: Option<LeafKind>, recorded: &Context,
    ) -> Result<(String, Declared), String> {
        let getter = Getter::parse(&check.getter);
        let exact = matches!(
            (self.ty.as_str(), getter, kind),
            ("string", Some(Getter::GetString), Some(LeafKind::Str))
                | ("integer", Some(Getter::GetInt | Getter::GetInt64), Some(LeafKind::I64))
                | ("boolean", Some(Getter::GetBool), Some(LeafKind::Bool))
        );
        if !exact {
            return Err(format!(
                "schema type {:?}, getter {} and ADP leaf kind {kind:?} have no exact comparison",
                self.ty, check.getter
            ));
        }

        let inputs = &case.inputs;
        if inputs.fleet_policy.is_some() || !inputs.cli.is_empty() {
            return Err("the case has a fleet policy or a CLI override".into());
        }
        if let Some(name) = inputs
            .env
            .iter()
            .flatten()
            .map(|(name, _)| name)
            .find(|name| self.env_vars.iter().any(|bound| bound.eq_ignore_ascii_case(name)))
        {
            return Err(format!("the case environment sets {name}, which can set the key"));
        }
        // Reject uncertain YAML, including escaped key names, rather than duplicating the YAML loader.
        if let Some(yaml) = &inputs.yaml {
            let segment = self.key.rsplit('.').next().unwrap_or(&self.key).to_lowercase();
            if yaml.to_lowercase().contains(&segment) || yaml.contains('\\') {
                return Err(format!(
                    "the case YAML mentions `{segment}` or holds an escape, so it may set the key"
                ));
            }
        }

        let Outcome::Started(started) = &case.outcome else {
            return Err("the case did not start".into());
        };
        let line = started
            .keys
            .iter()
            .find(|line| line.key == check.key)
            .ok_or("the case records no key line for the key")?;
        let read = &line.reads.snapshot;
        let streamed_source = line.snapshot.as_ref().map(|s| s.source);
        if read.source != Source::Default || streamed_source != Some(Source::Default) {
            return Err(format!(
                "the Agent reports source {:?} and streamed source {:?}, not both `default`",
                read.source.as_str(),
                streamed_source.map_or("<absent>", Source::as_str)
            ));
        }

        let (entry, value) = self.select(recorded);
        let value = value.ok_or_else(|| format!("{entry} has no value of type {:?}", self.ty))?;
        let streamed_value = line.snapshot.as_ref().and_then(|s| s.value.as_ref());
        let result = read.getters.iter().find(|g| g.getter == getter.expect("checked above"));
        if !streamed_value.is_some_and(|v| value.is_streamed(v)) || !result.is_some_and(|g| value.is_read(&g.result)) {
            return Err(format!(
                "for the recording ({recorded}) it selects {value} from {entry}, but the Agent streamed {} and its \
                 getter returned {}",
                streamed_value.map_or_else(|| "nothing".to_string(), ToString::to_string),
                result.map_or_else(|| "nothing".to_string(), |g| render_result(&g.result)),
            ));
        }
        Ok((entry, value))
    }
}

impl fmt::Display for Declaration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries: Vec<String> = self.entries.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        write!(
            f,
            "`{}` ({}) with platform_default {{{}}} in lib/datadog-agent/config/schema/core/",
            self.key,
            self.ty,
            entries.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use datadog_agent_config_corpus::BASELINE_CASE;
    use serde_json::json;

    use super::*;
    use crate::corpus_replay::corpus;
    use crate::corpus_replay::expectations::{
        collect, collect_for, intentional, judge, Collected, Entry, Judgement, Mismatch,
    };
    use crate::corpus_replay::expected::expectations;

    const SOCKET: &str = "dogstatsd_socket";
    const LINUX_SOCKET: &str = "\"/var/run/datadog/dsd.socket\"";

    fn bootstrap_check(origin: &str, getter: &str, key: &str) -> CheckKey {
        CheckKey {
            origin: origin.into(),
            tier: Tier::Bootstrap,
            at: "snapshot".into(),
            getter: getter.into(),
            key: key.into(),
        }
    }

    fn socket_check(origin: &str) -> CheckKey {
        bootstrap_check(origin, "GetString", SOCKET)
    }

    /// Failure messages for this check.
    fn naming<'a>(judgement: &'a Judgement, check: &CheckKey) -> Vec<&'a String> {
        let check = check.to_string();
        judgement.problems.iter().filter(|p| p.contains(&check)).collect()
    }

    /// Schema-generated expectations for this check.
    fn derived<'a>(collected: &'a Collected, check: &CheckKey) -> Vec<&'a Expectation> {
        collected
            .platform_defaults
            .iter()
            .filter(|e| e.check == *check)
            .collect()
    }

    /// The platform-default explanation attached to this check's result.
    #[track_caller]
    fn note<'a>(collected: &'a Collected, check: &CheckKey) -> &'a str {
        let result = collected.results.iter().find(|r| r.key == *check);
        let result = result.unwrap_or_else(|| panic!("{check} is checked"));
        result.note.as_deref().unwrap_or_else(|| panic!("{check} has no note"))
    }

    /// An OS whose socket default differs from this host's.
    fn other_socket_platform() -> &'static str {
        if cfg!(any(target_os = "linux", target_os = "aix")) {
            "darwin"
        } else {
            "linux"
        }
    }

    /// The native socket default, stated independently of ADP and the platform-default selector.
    fn host_socket_default() -> &'static str {
        if cfg!(any(target_os = "linux", target_os = "aix")) {
            LINUX_SOCKET
        } else {
            "\"\""
        }
    }

    /// The no-input baseline case expects the selected OS's socket default without adding or removing
    /// checks. Explicit YAML and environment inputs still require the recorded value.
    #[test]
    fn the_socket_default_follows_the_replay_platform_and_keeps_every_check() {
        let identities = |collected: &Collected| -> BTreeSet<String> {
            collected.results.iter().map(|r| r.key.to_string()).collect()
        };
        let host = identities(&collect(corpus()));
        for (goos, expected) in [
            ("linux", None),
            ("aix", None),
            ("darwin", Some("\"\"")),
            ("windows", Some("\"\"")),
            ("freebsd", Some("\"\"")),
        ] {
            let collected = collect_for(corpus(), &PlatformDefaults::for_goos(goos));
            assert_eq!(identities(&collected), host, "{goos}");

            let baseline = socket_check(BASELINE_CASE);
            let derived = derived(&collected, &baseline);
            match (derived.as_slice(), expected) {
                ([], None) => assert!(note(&collected, &baseline).contains("compares with the recorded Agent result")),
                ([e], Some(expected)) => {
                    assert_eq!(
                        (e.status, &e.adp),
                        (Status::Intentional, &Adp::value(expected)),
                        "{goos}"
                    );
                    let basis = e.at.to_string();
                    for part in [
                        "platform_defaults.rs",
                        "goos linux, not containerized, not on ECS Fargate",
                        &format!("goos {goos}, not containerized, not on ECS Fargate"),
                        "from `linux`",
                        "from `other`",
                    ] {
                        assert!(basis.contains(part), "{goos}: {basis}\n\nlacks: {part}");
                    }
                }
                (derived, expected) => panic!("{goos}: expected {expected:?}, derived {derived:#?}"),
            }
            assert_eq!(
                collected.platform_defaults.len(),
                derived.len(),
                "{goos}: only the baseline socket"
            );

            for (origin, reason) in [
                ("breadth-env", "the case environment sets DD_DOGSTATSD_SOCKET"),
                ("breadth-yaml", "the case YAML mentions `dogstatsd_socket`"),
            ] {
                let check = socket_check(origin);
                assert!(
                    note(&collected, &check).contains(reason),
                    "{goos}: {}",
                    note(&collected, &check)
                );
            }
            let noted: BTreeSet<&str> = collected
                .results
                .iter()
                .filter(|r| r.note.is_some())
                .map(|r| r.key.origin.as_str())
                .collect();
            assert_eq!(
                noted,
                BTreeSet::from([
                    "baseline-default",
                    "breadth-env",
                    "breadth-yaml",
                    "yaml-map-for-string-key"
                ]),
                "{goos}: only the socket's bootstrap checks are assessed"
            );
        }
    }

    /// Real bootstrap returns this host's socket default without a handwritten exception.
    #[test]
    fn bootstrap_expects_the_native_socket_default_of_this_host() {
        let collected = collect(corpus());
        let check = socket_check(BASELINE_CASE);
        let result = collected
            .results
            .iter()
            .find(|r| r.key == check)
            .expect("the baseline socket is checked");
        let derived: Vec<&Adp> = derived(&collected, &check).into_iter().map(|e| &e.adp).collect();
        if host_socket_default() == LINUX_SOCKET {
            assert!(
                result.mismatch.is_none() && derived.is_empty(),
                "{result:?} {derived:?}"
            );
        } else {
            let expected = Adp::value(host_socket_default());
            assert_eq!(result.mismatch.as_ref().map(|m| &m.adp), Some(&expected));
            assert_eq!(derived, [&expected]);
        }
        let judgement = judge(corpus(), &collected, &expectations());
        assert!(naming(&judgement, &check).is_empty(), "{:#?}", judgement.problems);
    }

    /// Judging native ADP output against the other OS family's default fails only the baseline socket.
    #[test]
    fn a_socket_default_for_the_wrong_platform_fails() {
        let goos = other_socket_platform();
        let collected = collect_for(corpus(), &PlatformDefaults::for_goos(goos));
        let judgement = judge(corpus(), &collected, &expectations());
        assert_eq!(judgement.problems.len(), 1, "{:#?}", judgement.problems);
        let problem = &judgement.problems[0];
        assert!(problem.contains(&socket_check(BASELINE_CASE).to_string()), "{problem}");
        assert!(problem.contains(&format!("for this host (goos {goos}")), "{problem}");
        let expected: &[&str] = if goos == "linux" {
            &["UNEXPECTED RESULT"]
        } else {
            &[
                "This check now matches the Agent.",
                "derived from the schema's platform default",
            ]
        };
        for part in expected {
            assert!(problem.contains(part), "{problem}\n\nlacks: {part}");
        }
    }

    /// Simulate a lost environment value by replacing ADP's collected result with its default.
    /// The judge must reject it without a platform exception.
    #[test]
    fn losing_an_explicit_socket_input_fails_without_allowance() {
        let mut collected = collect(corpus());
        let check = socket_check("breadth-env");
        assert!(derived(&collected, &check).is_empty());
        let result = collected
            .results
            .iter_mut()
            .find(|r| r.key == check)
            .expect("the env socket is checked");
        assert!(result.mismatch.is_none(), "ADP reads the explicit socket: {result:?}");
        result.mismatch = Some(Mismatch {
            adp: Adp::value(host_socket_default()),
        });
        let judgement = judge(corpus(), &collected, &expectations());
        let problems = naming(&judgement, &check);
        assert_eq!(problems.len(), 1, "{:#?}", judgement.problems);
        assert!(problems[0].contains("UNEXPECTED RESULT"), "{}", problems[0]);
        assert!(problems[0].contains("does not explain this check"), "{}", problems[0]);
    }

    /// Expected outcome of a baseline check using a modified test schema.
    enum Want {
        /// Both OS selections equal the recorded value, from this schema entry.
        Recorded(&'static str),
        /// The schema requires this value for replay; unmodified ADP must fail the check.
        Host(&'static str),
        /// Keep the ordinary comparison, with this reason in the diagnostic note.
        Unexplained(&'static str),
    }

    /// A named edit of the baseline case.
    type BaselineEdit<'a> = (&'a str, &'a dyn Fn(&mut Case));

    /// Runs real bootstrap with modified copies of the schema and baseline case.
    /// `contexts` names the case edits; each row specifies a context, key, getter and expected outcome.
    /// Only expectations use the modified schema and target macOS; ADP keeps its native defaults.
    #[track_caller]
    fn assert_fixture(
        declarations: &[(&str, Value)], contexts: &[BaselineEdit<'_>], rows: &[(&str, &str, &str, Want)],
    ) {
        let mut schema = vendored_schema();
        for (key, fields) in declarations {
            let node = schema["properties"][key]
                .as_object_mut()
                .unwrap_or_else(|| panic!("the schema declares {key}"));
            node.extend(fields.as_object().expect("fields are an object").clone());
        }
        let platform_defaults = PlatformDefaults::from_schema(&schema, "darwin");
        for (context, edit) in contexts {
            let mut corpus = corpus().clone();
            let baseline = corpus.cases.iter_mut().find(|c| c.name == BASELINE_CASE);
            edit(baseline.expect("the corpus has a baseline case"));
            let collected = collect_for(&corpus, &platform_defaults);
            let judgement = judge(&corpus, &collected, &expectations());
            for (_, key, getter, want) in rows.iter().filter(|row| row.0 == *context) {
                let check = bootstrap_check(BASELINE_CASE, getter, key);
                let (note, derived, problems) = (
                    note(&collected, &check),
                    derived(&collected, &check),
                    naming(&judgement, &check),
                );
                let at = format!("{context} {key}: {note}\n{derived:#?}\n{problems:#?}");
                match want {
                    Want::Recorded(entry) => {
                        assert!(derived.is_empty() && problems.is_empty(), "{at}");
                        assert!(note.contains(&format!("from {entry}, as recorded")), "{at}");
                        assert!(note.contains(&format!("from {entry}, so the check compares")), "{at}");
                    }
                    Want::Host(adp) => {
                        assert_eq!(derived.len(), 1, "{at}");
                        assert_eq!(
                            (derived[0].status, &derived[0].adp),
                            (Status::Intentional, &Adp::value(*adp)),
                            "{at}"
                        );
                        assert_eq!(problems.len(), 1, "{at}");
                        assert!(problems[0].contains("This check now matches the Agent."), "{at}");
                        assert!(
                            problems[0].contains("derived from the schema's platform default"),
                            "{at}"
                        );
                    }
                    Want::Unexplained(reason) => {
                        assert!(derived.is_empty() && problems.is_empty(), "{at}");
                        assert!(
                            note.contains("does not explain this check") && note.contains(reason),
                            "{at}"
                        );
                    }
                }
            }
        }
    }

    /// Covers Agent selection precedence and missing-entry fallbacks using schema fixtures.
    /// When a fixture requires a different host value, ADP's unchanged value must fail.
    #[test]
    fn fixture_declarations_select_as_the_agent_does() {
        let fargate_container: &dyn Fn(&mut Case) = &|case| {
            case.inputs.env = Some(BTreeMap::from([("ECS_FARGATE".into(), "true".into())]));
            let Outcome::Started(started) = &mut case.outcome else {
                panic!("the baseline case started");
            };
            started.containerized = true;
        };
        let declarations = [
            (
                "dogstatsd_pipe_name",
                json!({"platform_default": {"fargate": "", "container": "c", "linux": "l", "other": "o"}}),
            ),
            (
                "dogstatsd_capture_path",
                json!({"platform_default": {"container": "", "linux": "l", "darwin": "d", "other": "o"}}),
            ),
            (
                "dogstatsd_stream_socket",
                json!({"platform_default": {"linux": "", "darwin": "d"}}),
            ),
            (
                "dogstatsd_windows_pipe_security_descriptor",
                json!({"platform_default": {"windows": "w", "other": "D:AI(A;;GA;;;WD)"}}),
            ),
            (
                "dogstatsd_tag_cardinality",
                json!({"platform_default": {"linux": "low"}}),
            ),
            (
                "dogstatsd_port",
                json!({"platform_default": {"linux": 8125, "darwin": 9125}}),
            ),
            ("dogstatsd_buffer_size", json!({"platform_default": {"linux": 8192}})),
            (
                "dogstatsd_origin_detection",
                json!({"platform_default": {"linux": false, "other": true}}),
            ),
            (
                "dogstatsd_logging_enabled",
                json!({"platform_default": {"linux": true}}),
            ),
        ];
        use Want::*;
        assert_fixture(
            &declarations,
            &[("fargate and container", fargate_container), ("no context", &|_| {})],
            &[
                (
                    "fargate and container",
                    "dogstatsd_pipe_name",
                    "GetString",
                    Recorded("`fargate`"),
                ),
                (
                    "fargate and container",
                    "dogstatsd_capture_path",
                    "GetString",
                    Recorded("`container`"),
                ),
                (
                    "fargate and container",
                    "dogstatsd_stream_socket",
                    "GetString",
                    Host("\"d\""),
                ),
                (
                    "fargate and container",
                    "dogstatsd_windows_pipe_security_descriptor",
                    "GetString",
                    Recorded("`other`"),
                ),
                (
                    "no context",
                    "dogstatsd_pipe_name",
                    "GetString",
                    Unexplained("selects \"l\" from `linux`"),
                ),
                ("no context", "dogstatsd_tag_cardinality", "GetString", Host("\"\"")),
                ("no context", "dogstatsd_port", "GetInt", Host("9125")),
                ("no context", "dogstatsd_buffer_size", "GetInt", Host("0")),
                ("no context", "dogstatsd_origin_detection", "GetBool", Host("true")),
                ("no context", "dogstatsd_logging_enabled", "GetBool", Host("false")),
            ],
        );
    }

    /// Explicit or ambiguous inputs must keep the ordinary comparison, including alternate env
    /// bindings, mixed-case names and escaped YAML keys. Unrelated inputs do not exclude a key.
    #[test]
    fn inputs_that_may_set_a_key_leave_its_check_ordinary() {
        let declarations = [
            (
                "dogstatsd_port",
                json!({"platform_default": {"linux": 8125, "other": 1}}),
            ),
            (
                "dogstatsd_buffer_size",
                json!({"platform_default": {"linux": 8192, "other": 1}}),
            ),
            (
                "dogstatsd_expiry_seconds",
                json!({"platform_default": {"linux": 300, "other": 1}, "env_vars": ["DD_FIXTURE_EXPIRY"]}),
            ),
            (
                "dogstatsd_mapper_cache_size",
                json!({"platform_default": {"linux": 1000, "other": 1}}),
            ),
            (
                "dogstatsd_string_interner_size",
                json!({"platform_default": {"linux": 4096, "other": 1}}),
            ),
        ];
        let inputs: &dyn Fn(&mut Case) = &|case| {
            case.inputs.env = Some(BTreeMap::from([
                ("DD_DOGSTATSD_PORT".into(), "8125".into()),
                ("dd_dogstatsd_buffer_size".into(), "8192".into()),
                ("DD_FIXTURE_EXPIRY".into(), "300".into()),
            ]));
            case.inputs.yaml = Some("DogStatsD_Mapper_Cache_Size: 1000\n".into());
        };
        let escape: &dyn Fn(&mut Case) = &|case| {
            case.inputs.yaml = Some("\"dogstatsd\\x5fstring_interner_size\": 4096\n".into());
        };
        use Want::*;
        assert_fixture(
            &declarations,
            &[("inputs", inputs), ("escape", escape)],
            &[
                (
                    "inputs",
                    "dogstatsd_port",
                    "GetInt",
                    Unexplained("sets DD_DOGSTATSD_PORT"),
                ),
                (
                    "inputs",
                    "dogstatsd_buffer_size",
                    "GetInt",
                    Unexplained("sets dd_dogstatsd_buffer_size"),
                ),
                (
                    "inputs",
                    "dogstatsd_expiry_seconds",
                    "GetInt",
                    Unexplained("sets DD_FIXTURE_EXPIRY"),
                ),
                (
                    "inputs",
                    "dogstatsd_mapper_cache_size",
                    "GetInt",
                    Unexplained("mentions `dogstatsd_mapper_cache_size`"),
                ),
                ("inputs", "dogstatsd_string_interner_size", "GetInt", Host("1")),
                (
                    "escape",
                    "dogstatsd_string_interner_size",
                    "GetInt",
                    Unexplained("or holds an escape"),
                ),
            ],
        );
    }

    /// An Agent startup adjustment can still report `default` as its source. If the schema cannot
    /// reproduce the recorded value, the check must keep its ordinary comparison and explain why.
    #[test]
    fn a_recording_the_declaration_does_not_explain_fails_ordinarily() {
        let computed = "/opt/datadog-agent/run/dsd.socket";
        let mut corpus = corpus().clone();
        let Some(Case {
            outcome: Outcome::Started(started),
            ..
        }) = corpus.cases.iter_mut().find(|c| c.name == BASELINE_CASE)
        else {
            panic!("the baseline case started");
        };
        let line = started
            .keys
            .iter_mut()
            .find(|l| l.key == SOCKET)
            .expect("the baseline records the socket");
        line.snapshot.as_mut().expect("the socket is streamed").value = Some(json!(computed));
        line.reads.snapshot.getters[0].result = GetterResult::String(computed.into());

        let collected = collect(&corpus);
        let check = socket_check(BASELINE_CASE);
        assert!(derived(&collected, &check).is_empty());
        let judgement = judge(&corpus, &collected, &expectations());
        let problems = naming(&judgement, &check);
        assert_eq!(problems.len(), 1, "{:#?}", judgement.problems);
        for part in [
            "UNEXPECTED RESULT",
            "does not explain this check",
            "it selects \"/var/run/datadog/dsd.socket\" from `linux`, but the Agent streamed \
             \"/opt/datadog-agent/run/dsd.socket\" and its getter returned \"/opt/datadog-agent/run/dsd.socket\"",
        ] {
            assert!(problems[0].contains(part), "{}\n\nlacks: {part}", problems[0]);
        }
    }

    /// A handwritten expectation must fail as a duplicate even if it agrees with the generated one.
    #[test]
    fn a_handwritten_platform_default_entry_fails_as_a_duplicate() {
        let collected = collect_for(corpus(), &PlatformDefaults::for_goos("windows"));
        let check = socket_check(BASELINE_CASE);
        let derived = derived(&collected, &check);
        assert_eq!(derived.len(), 1, "{derived:#?}");
        for adp in ["\"\"", "None"] {
            let mut table = expectations();
            table.push(intentional(Entry {
                origin: BASELINE_CASE,
                tier: Tier::Bootstrap,
                at: "snapshot",
                getter: "GetString",
                key: SOCKET,
                adp: Adp::value(adp),
                cause: PLATFORM_DEFAULT,
            }));
            let judgement = judge(corpus(), &collected, &table);
            let duplicates: Vec<&String> = naming(&judgement, &check)
                .into_iter()
                .filter(|p| p.starts_with("DUPLICATE EXPECTATION"))
                .collect();
            assert_eq!(duplicates.len(), 1, "{adp}: {:#?}", judgement.problems);
            assert!(duplicates[0].contains(&derived[0].at.to_string()), "{}", duplicates[0]);
        }
    }
}
