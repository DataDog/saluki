use super::*;
use crate::export::{
    tests::{demo_export, DEMO_EXPORT},
    ExportBundle,
};
use crate::scenario::edits::append_to_metrics;

/// Compiles against the export alone, as for an ADP that does not serve its default program: the structural checks
/// run instead of composition. (Composition is tested in `preview::tests`, against a real default program.)
fn structural(export: &ExportBundle) -> CompileContext<'_> {
    CompileContext {
        export,
        default_program: None,
        active: None,
        others: &[],
        revision: 1,
        labels: &NO_LABELS,
        in_effect: None,
    }
}

/// No labels: the editor writes none beside the recipients.
static NO_LABELS: std::sync::LazyLock<RecipientLabels> = std::sync::LazyLock::new(RecipientLabels::default);

/// The demo export's artifact, as the editor prints it: hand-written, so it also pins the printed form the editor
/// starts from, without its base, and with the export's notes and recipient directory as comments. No line is longer
/// than the editor's width.
const DEFAULT_SOURCE: &str = r#"policy metrics {
  # Sources: each one's metrics enter at an input of the scope: dogstatsd.

  # Policy: one branch per output; route here
  let metrics = dogstatsd

  # Recipients:
  #   primary                   http://127.0.0.1:17101
  #   mrf                       http://127.0.0.1:17104
  #   "http://127.0.0.1:17102"
  #   "http://127.0.0.1:17103"
  metrics | emit output dd_metrics_encode

  # Endpoint routing to http://127.0.0.1:17102
  let metrics_routing_filter_0 = metrics
    | where intake_name in config.endpoint_routing.group.b0b0b0b0.names
        or intake_name starts_with config.endpoint_routing.group.b0b0b0b0.prefixes

  metrics_routing_filter_0 | emit output metrics_routing_encode_0

  # Endpoint routing to http://127.0.0.1:17103
  let metrics_routing_filter_1 = metrics
    | where intake_name in config.endpoint_routing.group.c0c0c0c0.names
        or intake_name starts_with config.endpoint_routing.group.c0c0c0c0.prefixes

  metrics_routing_filter_1 | emit output metrics_routing_encode_1

  # Multi-Region Failover: mirrors listed, or all, metrics while failing over
  let mrf_metrics_gateway = metrics
    | where config.multi_region_failover.enabled
        and config.multi_region_failover.metric_mirroring.enabled
        and (
          is_empty(config.multi_region_failover.metric_mirroring.allowlist)
          or name in config.multi_region_failover.metric_mirroring.allowlist
        )

  mrf_metrics_gateway | emit output mrf_metrics_encode
}
"#;

/// The failover gateway's branch of the policy, with the comment above it and the blank line before.
const MRF_BRANCH: &str =
    "\n  # Multi-Region Failover: mirrors listed, or all, metrics while failing over\n  let mrf_metrics_gateway = \
                          metrics\n";

/// Returns `source` without the failover gateway's branch.
fn without_mrf(source: &str) -> String {
    let start = source.find(MRF_BRANCH).expect("the source has the failover gateway");
    let end_text = "  mrf_metrics_gateway | emit output mrf_metrics_encode\n";
    let end = start + source[start..].find(end_text).unwrap() + end_text.len();
    format!("{}{}", &source[..start], &source[end..])
}

/// The demo export's base, with its `outputs` block, which the editor leaves out.
const BASE: &str = r#"  base "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"
  outputs {
    dd_metrics_encode = "sha256:2222222222222222222222222222222222222222222222222222222222222222",
    metrics_routing_encode_0 = "sha256:3333333333333333333333333333333333333333333333333333333333333333",
    metrics_routing_encode_1 = "sha256:4444444444444444444444444444444444444444444444444444444444444444",
    mrf_metrics_encode = "sha256:5555555555555555555555555555555555555555555555555555555555555555",
  }
"#;

/// The default source with its base and output fingerprints written out.
fn with_fingerprints() -> String {
    let head = "policy metrics {\n";
    format!("{}{}\n{}", head, BASE, &DEFAULT_SOURCE[head.len()..])
}

fn replaced(from: &str, to: &str) -> String {
    assert!(DEFAULT_SOURCE.contains(from), "{}", from);
    DEFAULT_SOURCE.replacen(from, to, 1)
}

fn replaced_in(source: &str, from: &str, to: &str) -> String {
    assert!(source.contains(from), "{}", from);
    source.replacen(from, to, 1)
}

#[test]
fn the_default_program_prints_compiles_without_findings_and_keeps_its_hash() {
    let export = demo_export();
    assert_eq!(default_program_source(&export, &NO_LABELS), DEFAULT_SOURCE);
    assert!(DEFAULT_SOURCE.lines().all(|line| line.chars().count() <= 120));

    let (outcome, compiled) = compile(DEFAULT_SOURCE, Some(&structural(&export)));
    assert!(outcome.ok);
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert_eq!((outcome.errors, outcome.warnings), (0, 0));
    assert!(outcome.checked_against.available);
    assert_eq!(
        outcome.checked_against.default_program_hash.as_deref(),
        Some("sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f")
    );
    // The fingerprints the editor leaves out come back from the export, so an unmodified export compiles to the
    // export's own document, and its hash is the export's.
    assert_eq!(
        outcome.restored_fingerprints,
        [
            "dd_metrics_encode",
            "metrics_routing_encode_0",
            "metrics_routing_encode_1",
            "mrf_metrics_encode"
        ]
    );
    let compiled = compiled.unwrap();
    assert_eq!(compiled.hash, export.artifact.hash);
    assert_eq!(outcome.artifact_hash, Some(export.artifact.hash.to_string()));
    assert_eq!(outcome.canonical_source.as_deref(), Some(DEFAULT_SOURCE));
    assert_eq!(
        ReceivedArtifact::from_value(compiled.document).unwrap().artifact,
        export.artifact.artifact
    );
    // Without the default program there is nothing to compose, so no plan diff, and the outcome says why.
    assert!(!outcome.checked_against.composed);
    assert!(!outcome.preview.available);
    assert!(
        outcome.preview.reason.contains("does not serve its default program"),
        "{}",
        outcome.preview.reason
    );

    // Written out, the fingerprints are kept as written, and printing keeps them.
    let written = with_fingerprints();
    let (outcome, compiled) = compile(&written, Some(&structural(&export)));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(outcome.restored_fingerprints.is_empty());
    assert_eq!(compiled.unwrap().hash, export.artifact.hash);
    assert_eq!(outcome.canonical_source.as_deref(), Some(written.as_str()));
}

#[test]
fn edits_change_the_hash_and_reformatting_does_not() {
    let export = demo_export();
    let reformatted = DEFAULT_SOURCE
        .replace("\n\n", "\n")
        .replace("  metrics | emit", "  metrics\n    | emit");
    let (outcome, _) = compile(&reformatted, Some(&structural(&export)));
    assert_eq!(outcome.artifact_hash, Some(export.artifact.hash.to_string()));
    assert_eq!(outcome.canonical_source.as_deref(), Some(DEFAULT_SOURCE));

    let edited = crate::scenario::edits::drop_debug(DEFAULT_SOURCE).unwrap();
    let (outcome, compiled) = compile(&edited, Some(&structural(&export)));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    let hash = compiled.unwrap().hash;
    assert_ne!(hash, export.artifact.hash);
    assert_eq!(outcome.artifact_hash, Some(hash.to_string()));

    // Removing a route removes its output from the restored fingerprints: an artifact lists what it refers to.
    let (outcome, _) = compile(&without_mrf(DEFAULT_SOURCE), Some(&structural(&export)));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(!outcome
        .restored_fingerprints
        .contains(&"mrf_metrics_encode".to_string()));
    assert_eq!(outcome.restored_fingerprints.len(), 3);

    // An output emitted to twice is restored once.
    let twice = replaced(
        "  metrics | emit output dd_metrics_encode\n",
        "  metrics | emit output dd_metrics_encode\n\n  metrics | where name starts_with \"demo.copy.\" | emit output \
         dd_metrics_encode\n",
    );
    let (outcome, _) = compile(&twice, Some(&structural(&export)));
    assert_eq!(
        outcome.restored_fingerprints,
        [
            "dd_metrics_encode",
            "metrics_routing_encode_0",
            "metrics_routing_encode_1",
            "mrf_metrics_encode"
        ],
        "{:#?}",
        outcome.diagnostics
    );
}

#[test]
fn the_examples_of_the_demo_scenarios_compile_without_findings() {
    // S2 of the design's scenarios: widen the checkout route to the platform team's metrics, written as the
    // implementation design writes it.
    let widened = replaced(
        "        or intake_name starts_with config.endpoint_routing.group.b0b0b0b0.prefixes\n",
        "        or intake_name starts_with config.endpoint_routing.group.b0b0b0b0.prefixes\n        or tags.team == \
         \"platform\"\n",
    );
    let (outcome, compiled) = compile(&widened, Some(&structural(&demo_export())));
    assert!(outcome.ok && compiled.is_some());
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
}

#[test]
fn syntax_errors_are_placed_by_line_and_column() {
    // `=>` instead of `|` on line 5: `=` is a token, and the `>` after it, at column 28, is not.
    let source = replaced("  let metrics = dogstatsd\n", "  let metrics = dogstatsd => tee\n");
    let (outcome, compiled) = compile(&source, Some(&structural(&demo_export())));
    assert!(!outcome.ok);
    assert!(compiled.is_none());
    assert_eq!(outcome.diagnostics.len(), 1, "{:#?}", outcome.diagnostics);
    let diagnostic = &outcome.diagnostics[0];
    assert_eq!(diagnostic.origin, Origin::Syntax);
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!((diagnostic.range.line, diagnostic.range.column), (5, 28));
    assert_eq!((diagnostic.range.end_line, diagnostic.range.end_column), (5, 29));
    assert_eq!(&source[diagnostic.range.start..diagnostic.range.end], ">");
    assert!(diagnostic.message.contains("`|`"), "{}", diagnostic.message);
    assert_eq!(outcome.errors, 1);
    assert!(outcome.artifact.is_none() && outcome.artifact_hash.is_none());
}

#[test]
fn documents_the_plan_crate_cannot_read_are_reported_at_the_start() {
    let source = replaced("policy metrics {\n", "policy metrics {\n  ir_version 99\n");
    let (outcome, compiled) = compile(&source, Some(&structural(&demo_export())));
    assert!(!outcome.ok);
    assert!(compiled.is_none());
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics[0].origin, Origin::Document);
    assert!(outcome.diagnostics[0]
        .message
        .contains("IR version 99 is not supported"));
    assert_eq!(
        (outcome.diagnostics[0].range.line, outcome.diagnostics[0].range.column),
        (1, 1)
    );
}

/// A diagnostic a case expects: its severity, its line, and a fragment of its message.
type Expected<'a> = (Severity, usize, &'a str);

#[test]
fn structural_findings_are_placed_where_the_problem_is() {
    let fingerprint_4 = "sha256:4444444444444444444444444444444444444444444444444444444444444444";
    let fingerprint_5 = "sha256:5555555555555555555555555555555555555555555555555555555555555555";
    let mrf_output_line = format!("    mrf_metrics_encode = \"{}\",\n", fingerprint_5);
    let mrf_output_fix = format!("Add `mrf_metrics_encode = \"{}\"`", fingerprint_5);
    let cases: Vec<(&str, String, Vec<Expected<'_>>)> = vec![
        (
            "an emit to an output ADP does not declare",
            replaced(
                "emit output metrics_routing_encode_1",
                "emit output metrics_routing_encode_9",
            ),
            vec![(
                Severity::Error,
                26,
                "`metrics_routing_encode_9`, which ADP does not declare",
            )],
        ),
        // Fingerprints written out are taken as written: an `outputs` list that leaves one out, or has a stale one,
        // is reported rather than completed from the export.
        (
            "an emit to an output the artifact does not list",
            replaced_in(&with_fingerprints(), &mrf_output_line, ""),
            vec![(Severity::Error, 44, mrf_output_fix.as_str())],
        ),
        (
            "an output whose fingerprint changed",
            replaced_in(
                &with_fingerprints(),
                fingerprint_4,
                "sha256:4444444444444444444444444444444444444444444444444444444444444445",
            ),
            vec![(Severity::Error, 6, "has fingerprint sha256:4444")],
        ),
        (
            "a binding ADP does not declare",
            replaced(
                "endpoint_routing.group.c0c0c0c0.names",
                "endpoint_routing.group.ffffffff.names",
            ),
            vec![(
                Severity::Error,
                23,
                "`endpoint_routing.group.ffffffff.names`, which ADP does not declare",
            )],
        ),
        (
            "a base that is not the current default program",
            replaced_in(&with_fingerprints(), "base \"sha256:0f0f", "base \"sha256:0e0f"),
            vec![(Severity::Warning, 2, "is not the current default program")],
        ),
        (
            "a parameter the operator does not have",
            append_to_metrics(
                DEFAULT_SOURCE,
                "namespace_prefix(\"x\", exclusions: [], colour: \"red\") as extra",
            )
            .unwrap(),
            vec![(Severity::Error, 6, "parameter `colour`")],
        ),
        (
            "an operator that is not registered",
            append_to_metrics(DEFAULT_SOURCE, "frobnicate(x: 1) as extra").unwrap(),
            vec![(Severity::Error, 6, "`frobnicate` is not a registered operator")],
        ),
        (
            "another scope",
            replaced("policy metrics {", "policy dogstatsd {"),
            vec![
                (Severity::Error, 1, "ADP exports the scope `metrics`"),
                (Severity::Error, 5, "has no input `dogstatsd`"),
            ],
        ),
    ];

    let export = demo_export();
    for (case, source, expected) in cases {
        let (outcome, compiled) = compile(&source, Some(&structural(&export)));
        // Findings never prevent publishing: ADP has the last word.
        assert!(outcome.ok, "{}: {:#?}", case, outcome.diagnostics);
        assert!(compiled.is_some(), "{}", case);
        let found = outcome
            .diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.severity, diagnostic.range.line))
            .collect::<Vec<_>>();
        let wanted = expected
            .iter()
            .map(|(severity, line, _)| (*severity, *line))
            .collect::<Vec<_>>();
        assert_eq!(found, wanted, "{}: {:#?}", case, outcome.diagnostics);
        for (diagnostic, (_, _, fragment)) in outcome.diagnostics.iter().zip(&expected) {
            assert_eq!(diagnostic.origin, Origin::Check, "{}", case);
            assert!(
                diagnostic.message.contains(fragment),
                "{}: {}",
                case,
                diagnostic.message
            );
        }
        let errors = expected
            .iter()
            .filter(|(severity, _, _)| *severity == Severity::Error)
            .count();
        assert_eq!(
            (outcome.errors, outcome.warnings),
            (errors, expected.len() - errors),
            "{}",
            case
        );
    }
}

#[test]
fn without_an_export_sources_are_parsed_but_not_checked() {
    // Without an export to give the base back, a source states its own.
    let with_base = replaced(
        "policy metrics {\n",
        "policy metrics {\n  base \"sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\"\n\n",
    );
    let (outcome, _) = compile(DEFAULT_SOURCE, None);
    assert!(!outcome.ok);
    assert!(
        outcome.diagnostics[0].message.contains("Expected `base"),
        "{:#?}",
        outcome.diagnostics
    );
    let source = replaced_in(
        &with_base,
        "emit output metrics_routing_encode_1",
        "emit output metrics_routing_encode_9",
    );
    let (outcome, compiled) = compile(&source, None);
    assert!(outcome.ok);
    assert!(compiled.is_some());
    assert!(outcome.diagnostics.is_empty());
    assert!(!outcome.checked_against.available);
    assert!(outcome.checked_against.reason.unwrap().contains("not available"));
    // With no export to take them from, left-out fingerprints stay out.
    assert!(outcome.restored_fingerprints.is_empty());
    assert_eq!(outcome.artifact.unwrap()["base"].get("outputs"), None);
}

#[test]
fn state_owners_are_checked_for_their_parameters_only() {
    let export = demo_export();
    // Aggregation with its parameters missing: each missing parameter is reported, at the node. That it keeps state is
    // no finding: ADP composes state owners, and placement decides where they run.
    let source = append_to_metrics(DEFAULT_SOURCE, "aggregate as rollup").unwrap();
    let (outcome, compiled) = compile(&source, Some(&structural(&export)));
    assert!(outcome.ok && compiled.is_some(), "{:#?}", outcome.diagnostics);
    assert!(!outcome.diagnostics.is_empty());
    for diagnostic in &outcome.diagnostics {
        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(diagnostic.range.line, 6);
        assert!(
            diagnostic
                .message
                .starts_with("Node `rollup` (`aggregate`): parameter `")
                && diagnostic.message.ends_with("`: missing required parameter"),
            "{}",
            diagnostic.message
        );
    }
    assert!(outcome
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("parameter `window`")));
}

#[test]
fn what_adp_installs_unchecked_is_not_reported() {
    // Until the gateways lower to routing operators, ADP's export holds them as opaque hosts, whose state is their host
    // component's: ADP installs its unmodified export, so the editor reports nothing about it.
    let mut document = serde_json::from_str::<serde_json::Value>(DEMO_EXPORT).unwrap();
    let all = serde_json::json!({"kinds": "all", "timestamps": "mixed"});
    let artifact = &mut document["artifact"];
    let nodes = artifact["nodes"].as_array_mut().unwrap();
    nodes.retain(|node| node["id"] != "mrf_metrics_gateway_false");
    let gateway = nodes
        .iter_mut()
        .find(|node| node["id"] == "mrf_metrics_gateway")
        .unwrap();
    gateway["op"] = serde_json::json!("opaque_host");
    gateway["params"] = serde_json::json!({"component": "mrf_metrics_gateway", "input": all, "outputs": {"out": all}});
    let edges = artifact["edges"].as_array_mut().unwrap();
    edges.retain(|edge| edge["from"] != "mrf_metrics_gateway.false");
    for edge in edges
        .iter_mut()
        .filter(|edge| edge["from"] == "mrf_metrics_gateway.true")
    {
        edge["from"] = serde_json::json!("mrf_metrics_gateway.out");
    }
    let export = ExportBundle::from_value(document).unwrap();
    let source = default_program_source(&export, &NO_LABELS);
    assert!(source.contains("opaque_host"), "{}", source);
    let (outcome, _) = compile(&source, Some(&structural(&export)));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);

    // ADP checks the fingerprints of the outputs an artifact emits to; one the base still records, but that nothing
    // emits to any more, cannot be removed under the artifact, whatever its fingerprint.
    let stale = "sha256:5555555555555555555555555555555555555555555555555555555555555556";
    let source = replaced_in(
        &with_fingerprints(),
        "sha256:5555555555555555555555555555555555555555555555555555555555555555",
        stale,
    );
    let source = without_mrf(&source);
    assert!(source.contains(stale));
    let (outcome, _) = compile(&source, Some(&structural(&demo_export())));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
}

#[test]
fn columns_count_characters_and_offsets_past_the_end_are_clamped() {
    let source = "a\n# é ü\nbcd";
    let index = LineIndex::new(source);
    assert_eq!(index.position(0), (1, 1));
    assert_eq!(index.position(1), (1, 2));
    assert_eq!(index.position(2), (2, 1));
    // `ü` starts at byte 7 (after `# `, two-byte `é`, and a space): the fifth character of line 2.
    assert_eq!(index.position(7), (2, 5));
    // The middle of a multi-byte character counts as its start.
    assert_eq!(index.position(8), (2, 5));
    assert_eq!(index.position(source.len()), (3, 4));
    assert_eq!(index.position(source.len() + 10), (3, 4));
    let range = index.range(Some(Span::new(2, 12)));
    assert_eq!(
        (range.line, range.column, range.end_line, range.end_column),
        (2, 1, 3, 3)
    );
}

#[test]
fn the_recipient_directory_names_the_demos_labels_and_what_the_program_addresses_by_name() {
    // The detached export addresses outputs by their recipients' names: `emit [...]`.
    let mut document = serde_json::from_str::<serde_json::Value>(DEMO_EXPORT).unwrap();
    for node in document["artifact"]["nodes"].as_array_mut().unwrap() {
        if node["id"] == "metrics_policy_0" {
            node["params"] = serde_json::json!({"to": ["primary", "http://127.0.0.1:17103"]});
        }
    }
    let export = ExportBundle::from_value(document).unwrap();
    let labels = RecipientLabels::new(vec![
        ("http://127.0.0.1:17101".to_string(), "Primary".to_string()),
        ("http://127.0.0.1:17103".to_string(), "Team C".to_string()),
    ]);
    let source = default_program_source(&export, &labels);
    let directory = source
        .lines()
        .skip_while(|line| *line != "  # Recipients:")
        .take(5)
        .collect::<Vec<_>>();
    assert_eq!(
        directory,
        [
            "  # Recipients:",
            "  #   primary                   Primary  http://127.0.0.1:17101",
            "  #   mrf                                http://127.0.0.1:17104",
            "  #   \"http://127.0.0.1:17102\"",
            "  #   \"http://127.0.0.1:17103\"  Team C",
        ],
        "{}",
        source
    );
    // Without the routing group's emit to it, team B's endpoint is not addressed.
    let unrouted = source
        .replace(
            "  metrics_routing_filter_0 | emit output metrics_routing_encode_0\n",
            "  metrics_routing_filter_0 | drop\n",
        )
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{}\n", line))
        .collect::<String>();
    let artifact = saluki_metric_plan_syntax::parse_with(
        &unrouted,
        &ParseOptions {
            base: Some(export.artifact.artifact.base.clone()),
        },
    )
    .unwrap()
    .artifact;
    let annotated = annotate(&unrouted, &artifact, Some(&export), &labels);
    assert!(
        annotated
            .lines()
            .any(|line| line.starts_with("  #   \"http://127.0.0.1:17102\"") && line.ends_with("  (not addressed)")),
        "{}",
        annotated
    );
    // The comments are not part of the artifact.
    let parse = |text: &str| {
        saluki_metric_plan_syntax::parse_with(
            text,
            &ParseOptions {
                base: Some(export.artifact.artifact.base.clone()),
            },
        )
        .unwrap()
        .artifact
    };
    assert_eq!(parse(&source), parse(&default_program_source(&export, &NO_LABELS)));
}

#[test]
fn what_the_export_leaves_out_is_one_comment_at_the_top_wrapped_to_the_editors_width() {
    let mut document = serde_json::from_str::<serde_json::Value>(DEMO_EXPORT).unwrap();
    let note = "Not shown, because they do nothing as configured: mapper (no profiles), namespace prefix (none), \
                metric filterlist (empty), tag filterlist (empty), post-aggregation filter (empty), \
                enrichment merge (one input), host enrichment (changes no metric)";
    document["annotations"]["omitted"] = serde_json::json!(note);
    let export = ExportBundle::from_value(document).unwrap();
    let source = default_program_source(&export, &NO_LABELS);
    let lines = source.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "policy metrics {");
    let comment = lines[1..]
        .iter()
        .take_while(|line| !line.is_empty())
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        comment,
        [
            "  # Not shown, because they do nothing as configured: mapper (no profiles), namespace prefix (none),",
            "  #   metric filterlist (empty), tag filterlist (empty), post-aggregation filter (empty), enrichment",
            "  #   merge (one input), host enrichment (changes no metric)",
        ],
        "{}",
        source
    );
    assert!(comment.iter().all(|line| line.chars().count() <= 100), "{}", source);
    // A blank line, then the line that names the inputs.
    assert_eq!(
        lines[1 + comment.len() + 1],
        "  # Sources: each one's metrics enter at an input of the scope: dogstatsd.",
        "{}",
        source
    );
    // The comment is not part of the artifact.
    let parse = |text: &str| {
        saluki_metric_plan_syntax::parse_with(
            text,
            &ParseOptions {
                base: Some(export.artifact.artifact.base.clone()),
            },
        )
        .unwrap()
        .artifact
    };
    let plain = source
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{}\n", line))
        .collect::<String>();
    assert_eq!(parse(&source), parse(&plain));
}

#[test]
fn the_recipient_directory_goes_above_the_first_use_whole_when_it_runs_over_several_lines() {
    let export = demo_export();
    let source = default_program_source(&export, &NO_LABELS);
    let bare = "  metrics | emit output dd_metrics_encode\n";
    assert!(source.contains(bare), "{}", source);
    // The first use is a multi-line bare pipeline, so its stages are not at the start of a line of their own.
    let plain = source
        .replace(
            bare,
            "  metrics\n    | where true\n    | emit output dd_metrics_encode\n",
        )
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{}\n", line))
        .collect::<String>();
    let artifact = saluki_metric_plan_syntax::parse_with(
        &plain,
        &ParseOptions {
            base: Some(export.artifact.artifact.base.clone()),
        },
    )
    .unwrap()
    .artifact;
    let annotated = annotate(&plain, &artifact, Some(&export), &NO_LABELS);
    let lines = annotated.lines().collect::<Vec<_>>();
    let statement = lines
        .iter()
        .position(|line| *line == "  metrics")
        .expect("the statement");
    assert_eq!(lines[statement + 1], "    | where true", "{}", annotated);
    assert_eq!(
        lines[statement + 2],
        "    | emit output dd_metrics_encode",
        "{}",
        annotated
    );
    let heading = lines
        .iter()
        .position(|line| *line == "  # Recipients:")
        .expect("the directory");
    assert!(heading < statement, "{}", annotated);
    assert!(
        lines[heading..statement].iter().all(|line| line.starts_with("  #")),
        "the directory sits directly above the statement: {}",
        annotated
    );
}
