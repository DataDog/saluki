//! Checks of the config recorder's corpus (`lib/datadog-agent/config-recorder/corpus.jsonl`) that
//! need neither Go nor Docker: its format, its size, and that it is current with the vendored
//! schema, the overlay and the recorder's own inputs.
//!
//! Every check reads the corpus through `datadog_agent_config_corpus::read`, the only parser of it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use datadog_agent_config_corpus::{Case, Corpus, Group, Inputs, Outcome, Source, REVIEWED_AT_AGENT_COMMIT};
use datadog_agent_config_overlay_model::schema_gen::{load_schema_from_value, EnvBinding};
use datadog_agent_config_overlay_model::{load_resolved_schema, Files, KnownEntry, SchemaOverlay};
use sha2::{Digest, Sha256};

const SIZE_CAP: usize = 512_000;

/// The command that regenerates the corpus, named in every staleness failure.
const REGENERATE: &str = "make build-agent-config-corpus";

/// Added to failures that regeneration may not fix.
const DISAGREE: &str = "if it still fails after regenerating, the reader's lists or the generator disagree with the \
                        contract";

/// Keys an env-only `breadth` or `unsupported` case may stream from a source other than
/// `environment-variable`, each with the reason.
const ENV_SOURCE_EXCEPTIONS: &[(&str, &str)] = &[];

/// `lib/datadog-agent/config/`, where this crate lives.
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `lib/datadog-agent/config-recorder/`.
fn recorder_dir() -> PathBuf {
    crate_dir().join("..").join("config-recorder")
}

fn corpus_bytes() -> Vec<u8> {
    let path = recorder_dir().join("corpus.jsonl");
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}; run `{REGENERATE}`", path.display()))
}

/// The corpus model, or `None` when it breaks the format. `corpus_lines_read_strictly` reports
/// that case, so the other checks step aside and each break fails exactly one test.
fn corpus() -> Option<Corpus> {
    match datadog_agent_config_corpus::read(&corpus_bytes()) {
        Ok(c) => Some(c),
        Err(v) => {
            eprintln!(
                "skipped: the corpus breaks {} format rule(s); see corpus_lines_read_strictly",
                v.len()
            );
            None
        }
    }
}

#[test]
fn corpus_lines_read_strictly() {
    let Err(violations) = datadog_agent_config_corpus::read(&corpus_bytes()) else {
        return;
    };
    let shown: Vec<String> = violations.iter().map(ToString::to_string).collect();
    panic!(
        "corpus.jsonl breaks {} format rule(s); regenerate it with `{REGENERATE}`; {DISAGREE}:\n{}",
        violations.len(),
        shown.join("\n")
    );
}

#[test]
fn corpus_within_size_cap() {
    let len = corpus_bytes().len();
    assert!(
        len <= SIZE_CAP,
        "corpus.jsonl is {len} bytes, over the {SIZE_CAP}-byte cap; shrink the cases and run `{REGENERATE}`"
    );
}

/// Both pin facts, checked independently so a schema bump reports both at once instead of hiding
/// the reviewed-lists check behind the staleness one. `recorded` is `None` when the corpus itself
/// broke the format (`corpus_lines_read_strictly` already reports that).
fn pin_problems(recorded: Option<&str>, reviewed: &str, pin: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(recorded) = recorded {
        if recorded != pin {
            problems.push(format!(
                "the corpus was recorded at Agent commit {recorded}, but _version.txt pins {pin}; the corpus is \
                 stale; run `{REGENERATE}`"
            ));
        }
    }
    if reviewed != pin {
        problems.push(format!(
            "_version.txt pins Agent commit {pin}, but the corpus reader's lists of sources, getters and groups \
             were reviewed at {reviewed}; re-check them in datadog-agent-config-corpus against \
             pkg/config/model/types.go at the new pin, then bump REVIEWED_AT_AGENT_COMMIT"
        ));
    }
    problems
}

#[test]
fn corpus_pin_matches_vendored_schema() {
    let path = crate_dir().join("schema").join("core").join("_version.txt");
    let pin = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let pin = pin.trim();
    let recorded = corpus().map(|c| c.header.agent_commit);
    let problems = pin_problems(recorded.as_deref(), REVIEWED_AT_AGENT_COMMIT, pin);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[cfg(test)]
mod pin_problems_tests {
    use super::pin_problems;

    #[test]
    fn both_facts_are_named_when_both_are_wrong() {
        let problems = pin_problems(Some("aaa"), "bbb", "ccc");
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert!(problems[0].contains("corpus is"), "{problems:#?}");
        assert!(problems[0].contains("make build-agent-config-corpus"), "{problems:#?}");
        assert!(problems[1].contains("re-check them"), "{problems:#?}");
    }

    #[test]
    fn no_problems_when_both_facts_agree() {
        assert!(pin_problems(Some("aaa"), "aaa", "aaa").is_empty());
    }

    #[test]
    fn missing_corpus_skips_only_the_staleness_check() {
        assert_eq!(pin_problems(None, "aaa", "aaa").len(), 0);
        assert_eq!(pin_problems(None, "bbb", "aaa").len(), 1);
    }
}

/// Collects regular files below `dir`, skipping any path component that starts with `.`.
fn collect_files(root: &Path, rel: &str, out: &mut Vec<String>) {
    let dir = root.join(rel);
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
        let name = entry.file_name().into_string().expect("recorder paths are UTF-8");
        if name.starts_with('.') {
            continue;
        }
        let path = format!("{rel}/{name}");
        // `find -type f` does not follow symlinks, so neither does this.
        let kind = entry.file_type().expect("file type");
        if kind.is_dir() {
            collect_files(root, &path, out);
        } else if kind.is_file() {
            out.push(path);
        }
    }
}

#[test]
fn corpus_inputs_digest_is_current() {
    let root = recorder_dir();
    let mut files = vec!["agent_codegen.py".to_string(), "regenerate.sh".to_string()];
    collect_files(&root, "go", &mut files);
    collect_files(&root, "cases", &mut files);
    files.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut listing = String::new();
    for f in &files {
        let bytes = std::fs::read(root.join(f)).unwrap_or_else(|e| panic!("cannot read {f}: {e}"));
        let hex: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        listing.push_str(&format!("{hex}  {f}\n"));
    }
    let digest: String = Sha256::digest(listing.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let expected = format!("sha256:{digest}");
    let Some(corpus) = corpus() else { return };
    let recorded = corpus.header.inputs_digest;
    assert_eq!(
        recorded, expected,
        "the recorder's inputs (go/, cases/, agent_codegen.py, regenerate.sh) changed since the corpus was \
         recorded; the corpus is stale, run `{REGENERATE}`"
    );
}

/// How a case sets its keys, from its `inputs`. YAML breadth cases also carry `set` updates.
fn input_source(inputs: &Inputs) -> &'static str {
    let other = inputs.fleet_policy.is_some() || !inputs.cli.is_empty();
    match (
        inputs.env.is_some(),
        inputs.yaml.is_some(),
        inputs.updates.is_empty(),
        other,
    ) {
        (false, false, true, false) => "none",
        (true, false, true, false) => "env",
        (false, true, _, false) => "yaml",
        _ => "other",
    }
}

/// The key lines of a case that started; a startup error records none.
fn key_lines(case: &Case) -> impl Iterator<Item = &datadog_agent_config_corpus::KeyLine> {
    let keys = match &case.outcome {
        Outcome::Started(s) => s.keys.as_slice(),
        Outcome::StartupError(_) => &[],
    };
    keys.iter()
}

#[test]
fn corpus_groups_match_overlay() {
    let Some(corpus) = corpus() else { return };
    let files = Files::default();
    let schema = load_resolved_schema(&files.datadog_schema).unwrap_or_else(|e| panic!("schema: {e}"));
    let fields = load_schema_from_value(&schema);
    let leaves: BTreeSet<String> = fields.keys().map(|k| k.to_lowercase()).collect();
    let env_leaves: BTreeSet<String> = fields
        .iter()
        .filter(|(_, f)| !matches!(f.env, EnvBinding::None))
        .map(|(k, _)| k.to_lowercase())
        .collect();
    let overlay = SchemaOverlay::load(files).unwrap_or_else(|e| panic!("overlay: {e}"));
    let mut modeled = BTreeSet::new();
    let mut unsupported = BTreeSet::new();
    for (key, entry) in &overlay.inventory {
        let key = key.to_lowercase();
        match entry {
            KnownEntry::Full(_) | KnownEntry::Partial(_) => modeled.insert(key),
            KnownEntry::Unsupported(_) | KnownEntry::Unknown(_) => unsupported.insert(key),
        };
    }
    let excluded: BTreeSet<String> = overlay.excluded.keys().map(|k| k.to_lowercase()).collect();
    let adp_class = |k: &str| -> &'static str {
        if !leaves.contains(k) {
            "unknown (not a schema leaf)"
        } else if modeled.contains(k) {
            "baseline/breadth (modeled)"
        } else if unsupported.contains(k) {
            "unsupported"
        } else if excluded.contains(k) {
            "excluded"
        } else {
            "unclassified"
        }
    };

    // (group, source) -> keys, and key -> the groups that record it.
    let mut sets: BTreeMap<(Group, &'static str), BTreeSet<String>> = BTreeMap::new();
    let mut groups_of: BTreeMap<String, BTreeSet<Group>> = BTreeMap::new();
    let mut problems = Vec::new();
    for case in &corpus.cases {
        let source = input_source(&case.inputs);
        let expected_source = match case.group {
            Group::Baseline => Some(&["none"][..]),
            Group::Breadth | Group::Unsupported | Group::Excluded | Group::Unknown => Some(&["env", "yaml"][..]),
            Group::Depth | Group::Behavior => None,
        };
        if expected_source.is_some_and(|s| !s.contains(&source)) {
            problems.push(format!(
                "case {}: group {} with inputs of kind {source}",
                case.name, case.group
            ));
        }
        for line in key_lines(case) {
            let key = line.key.to_lowercase();
            groups_of.entry(key.clone()).or_default().insert(case.group);
            sets.entry((case.group, source)).or_default().insert(key);
        }
    }

    let recorded = |group: Group, source: &'static str| sets.get(&(group, source)).cloned().unwrap_or_default();
    let describe = |k: &str| {
        let actual = groups_of
            .get(k)
            .map(|g| g.iter().map(|g| g.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "absent".into());
        format!("  {k}: expected {}, actual {actual}", adp_class(k))
    };
    let mut compare = |label: &str, expected: BTreeSet<String>, actual: BTreeSet<String>| {
        for (what, diff) in [
            ("missing from", &expected - &actual),
            ("unexpected in", &actual - &expected),
        ] {
            if !diff.is_empty() {
                let shown: Vec<String> = diff.iter().take(20).map(|k| describe(k)).collect();
                problems.push(format!("{} key(s) {what} {label}:\n{}", diff.len(), shown.join("\n")));
            }
        }
    };
    let modeled_leaves = &modeled & &leaves;
    let unsupported_leaves = &unsupported & &leaves;
    compare("baseline", modeled_leaves.clone(), recorded(Group::Baseline, "none"));
    compare(
        "breadth yaml cases",
        modeled_leaves.clone(),
        recorded(Group::Breadth, "yaml"),
    );
    compare(
        "breadth env cases",
        &modeled_leaves & &env_leaves,
        recorded(Group::Breadth, "env"),
    );
    compare(
        "unsupported yaml cases",
        unsupported_leaves.clone(),
        recorded(Group::Unsupported, "yaml"),
    );
    compare(
        "unsupported env cases",
        &unsupported_leaves & &env_leaves,
        recorded(Group::Unsupported, "env"),
    );
    let excluded_keys = &recorded(Group::Excluded, "env") | &recorded(Group::Excluded, "yaml");
    let excluded_leaves = &excluded & &leaves;
    compare(
        "excluded cases (keys that are no longer excluded leaves)",
        &excluded_keys & &excluded_leaves,
        excluded_keys.clone(),
    );
    let unknown_keys = &recorded(Group::Unknown, "env") | &recorded(Group::Unknown, "yaml");
    compare(
        "unknown cases (keys that are schema leaves)",
        &unknown_keys - &leaves,
        unknown_keys.clone(),
    );

    assert!(
        problems.is_empty(),
        "the corpus's case groups disagree with schema_overlay.yaml and the vendored schema; the corpus may be \
         stale, run `{REGENERATE}`; {DISAGREE}:\n{}",
        problems.join("\n")
    );
}

#[test]
fn corpus_env_cases_stream_env_source() {
    let Some(corpus) = corpus() else { return };
    let mut env_cases = 0;
    let mut violations = BTreeMap::new();
    for case in &corpus.cases {
        let env_only = matches!(case.group, Group::Breadth | Group::Unsupported) && input_source(&case.inputs) == "env";
        if !env_only {
            continue;
        }
        env_cases += 1;
        for line in key_lines(case) {
            let source = line.snapshot.as_ref().map(|s| s.source);
            if source != Some(Source::EnvironmentVariable) {
                let shown = source.map_or("absent", Source::as_str);
                violations.insert(line.key.clone(), format!("case {}: streamed source {shown}", case.name));
            }
        }
    }
    assert!(
        env_cases > 0,
        "the corpus has no env-only breadth or unsupported case; run `{REGENERATE}`; {DISAGREE}"
    );
    let excepted: BTreeSet<&str> = ENV_SOURCE_EXCEPTIONS.iter().map(|(k, _)| *k).collect();
    let unexpected: Vec<String> = violations
        .iter()
        .filter(|(k, _)| !excepted.contains(k.as_str()))
        .map(|(k, v)| format!("  {k}: {v}"))
        .collect();
    let stale: Vec<&str> = excepted
        .iter()
        .filter(|k| !violations.contains_key(**k))
        .copied()
        .collect();
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "env-only breadth/unsupported keys must stream source environment-variable; check the key's env name \
         against the Agent and run `{REGENERATE}`:\n{}\nexceptions that no longer apply: {stale:?}",
        unexpected.join("\n")
    );
}
