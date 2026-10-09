use super::*;
use crate::export::tests::demo_export;

/// The demo export's artifact, as the editor prints it: hand-written, so it also pins the printed form the editor
/// starts from. No line is longer than 80 characters, and the lines an author edits (routing predicates, binding
/// names, emits) are at most 74.
const DEFAULT_SOURCE: &str = r#"policy post_enrich {
  base "sha256:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"

  input.metrics -> metrics_policy

  node metrics_policy = tee {
    -> emit dd_metrics_encode
    -> metrics_routing_filter_0
    -> metrics_routing_filter_1
    -> mrf_metrics_gateway
  }

  node metrics_routing_filter_0 = split(
    intake_name matches(
      values: config("endpoint_routing.group.b0b0b0b0.names"),
      prefix: false,
    )
    or intake_name matches(
      values: config("endpoint_routing.group.b0b0b0b0.prefixes"),
      prefix: true,
    )
  ) {
    true  -> emit metrics_routing_encode_0
    false -> drop
  }

  node metrics_routing_filter_1 = split(
    intake_name matches(
      values: config("endpoint_routing.group.c0c0c0c0.names"),
      prefix: false,
    )
    or intake_name matches(
      values: config("endpoint_routing.group.c0c0c0c0.prefixes"),
      prefix: true,
    )
  ) {
    true  -> emit metrics_routing_encode_1
    false -> drop
  }

  node mrf_metrics_gateway = split(
    flag(config("multi_region_failover.enabled"))
    and flag(config("multi_region_failover.metric_mirroring.enabled"))
    and (
      is_empty(config("multi_region_failover.metric_mirroring.allowlist"))
      or name matches(
        values: config("multi_region_failover.metric_mirroring.allowlist"),
        prefix: false,
      )
    )
  ) {
    true  -> emit mrf_metrics_encode
    false -> drop
  }
}
"#;

/// The `outputs` block of the demo export's base, which the editor leaves out.
const OUTPUTS: &str = r#"  outputs {
    dd_metrics_encode = "sha256:2222222222222222222222222222222222222222222222222222222222222222",
    metrics_routing_encode_0 = "sha256:3333333333333333333333333333333333333333333333333333333333333333",
    metrics_routing_encode_1 = "sha256:4444444444444444444444444444444444444444444444444444444444444444",
    mrf_metrics_encode = "sha256:5555555555555555555555555555555555555555555555555555555555555555",
  }
"#;

/// The default source with its output fingerprints written out, after `base`.
fn with_fingerprints() -> String {
    let base_end = DEFAULT_SOURCE.find("\"\n").unwrap() + 2;
    format!(
        "{}{}{}",
        &DEFAULT_SOURCE[..base_end],
        OUTPUTS,
        &DEFAULT_SOURCE[base_end..]
    )
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
    assert_eq!(default_program_source(&export), DEFAULT_SOURCE);
    assert!(DEFAULT_SOURCE.lines().all(|line| line.chars().count() <= 80));

    let (outcome, compiled) = compile(DEFAULT_SOURCE, Some(&export));
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
    assert!(!outcome.preview.available);
    assert!(outcome.preview.reason.contains("ADP validates the composed plan"));

    // Written out, the fingerprints are kept as written, and printing keeps them.
    let written = with_fingerprints();
    let (outcome, compiled) = compile(&written, Some(&export));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(outcome.restored_fingerprints.is_empty());
    assert_eq!(compiled.unwrap().hash, export.artifact.hash);
    assert_eq!(outcome.canonical_source.as_deref(), Some(written.as_str()));
}

#[test]
fn edits_change_the_hash_and_reformatting_does_not() {
    let export = demo_export();
    let reformatted = DEFAULT_SOURCE.replace("\n\n", "\n").replace("true  ->", "true ->");
    let (outcome, _) = compile(&reformatted, Some(&export));
    assert_eq!(outcome.artifact_hash, Some(export.artifact.hash.to_string()));
    assert_eq!(outcome.canonical_source.as_deref(), Some(DEFAULT_SOURCE));

    let edited = replaced(
        "  input.metrics -> metrics_policy\n",
        "  input.metrics -> drop_debug\n\n  node drop_debug = split(name starts_with \"demo.debug.\") {\n    true  -> \
         drop\n    false -> metrics_policy\n  }\n",
    );
    let (outcome, compiled) = compile(&edited, Some(&export));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    let hash = compiled.unwrap().hash;
    assert_ne!(hash, export.artifact.hash);
    assert_eq!(outcome.artifact_hash, Some(hash.to_string()));

    // Removing a route removes its output from the restored fingerprints: an artifact lists what it refers to.
    let without_mrf = replaced("    -> mrf_metrics_gateway\n", "");
    let without_mrf = without_mrf.replace(&DEFAULT_SOURCE[DEFAULT_SOURCE.find("\n  node mrf").unwrap()..], "\n}\n");
    let (outcome, _) = compile(&without_mrf, Some(&export));
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
    assert!(!outcome
        .restored_fingerprints
        .contains(&"mrf_metrics_encode".to_string()));
    assert_eq!(outcome.restored_fingerprints.len(), 3);
}

#[test]
fn the_examples_of_the_demo_scenarios_compile_without_findings() {
    // S2 of the design's scenarios: widen the checkout route to the platform team's metrics, written as the
    // implementation design writes it.
    let widened = replaced(
        "    or intake_name matches(\n      values: config(\"endpoint_routing.group.b0b0b0b0.prefixes\"),\n      \
         prefix: true,\n    )\n",
        "    or intake_name matches(\n      values: config(\"endpoint_routing.group.b0b0b0b0.prefixes\"),\n      \
         prefix: true,\n    )\n    or tag(\"team\") == \"platform\"\n",
    );
    let (outcome, compiled) = compile(&widened, Some(&demo_export()));
    assert!(outcome.ok && compiled.is_some());
    assert!(outcome.diagnostics.is_empty(), "{:#?}", outcome.diagnostics);
}

#[test]
fn syntax_errors_are_placed_by_line_and_column() {
    // `=>` instead of `->` on line 7: `=` is a token, and the `>` after it, at column 6, is not.
    let source = replaced("    -> emit dd_metrics_encode", "    => emit dd_metrics_encode");
    let (outcome, compiled) = compile(&source, Some(&demo_export()));
    assert!(!outcome.ok);
    assert!(compiled.is_none());
    assert_eq!(outcome.diagnostics.len(), 1, "{:#?}", outcome.diagnostics);
    let diagnostic = &outcome.diagnostics[0];
    assert_eq!(diagnostic.origin, Origin::Syntax);
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!((diagnostic.range.line, diagnostic.range.column), (7, 6));
    assert_eq!((diagnostic.range.end_line, diagnostic.range.end_column), (7, 7));
    assert_eq!(&source[diagnostic.range.start..diagnostic.range.end], ">");
    assert!(diagnostic.message.contains("->"), "{}", diagnostic.message);
    assert_eq!(outcome.errors, 1);
    assert!(outcome.artifact.is_none() && outcome.artifact_hash.is_none());
}

#[test]
fn documents_the_plan_crate_cannot_read_are_reported_at_the_start() {
    let source = replaced("policy post_enrich {\n", "policy post_enrich {\n  ir_version 99\n");
    let (outcome, compiled) = compile(&source, Some(&demo_export()));
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
            replaced("emit metrics_routing_encode_1", "emit metrics_routing_encode_9"),
            vec![(
                Severity::Error,
                37,
                "`metrics_routing_encode_9`, which ADP does not declare",
            )],
        ),
        // Fingerprints written out are taken as written: an `outputs` list that leaves one out, or has a stale one,
        // is reported rather than completed from the export.
        (
            "an emit to an output the artifact does not list",
            replaced_in(&with_fingerprints(), &mrf_output_line, ""),
            vec![(Severity::Error, 57, mrf_output_fix.as_str())],
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
                28,
                "`endpoint_routing.group.ffffffff.names`, which ADP does not declare",
            )],
        ),
        (
            "a base that is not the current default program",
            replaced("base \"sha256:0f0f", "base \"sha256:0e0f"),
            vec![(Severity::Warning, 2, "is not the current default program")],
        ),
        (
            "a parameter the operator does not have",
            replaced(
                "\n}\n",
                "\n\n  node extra = tee(ports: 1, colour: \"red\") {\n    0 -> drop\n  }\n}\n",
            ),
            vec![(Severity::Error, 56, "parameter `colour`")],
        ),
        (
            "an operator that is not registered",
            replaced("\n}\n", "\n\n  node extra = frobnicate(x: 1)\n}\n"),
            vec![(Severity::Error, 56, "`frobnicate` is not a registered operator")],
        ),
        (
            "another scope",
            replaced("policy post_enrich {", "policy dogstatsd {"),
            vec![
                (Severity::Error, 1, "ADP exports the scope `post_enrich`"),
                (Severity::Error, 4, "has no input `metrics`"),
            ],
        ),
    ];

    let export = demo_export();
    for (case, source, expected) in cases {
        let (outcome, compiled) = compile(&source, Some(&export));
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
    let source = replaced("emit metrics_routing_encode_1", "emit metrics_routing_encode_9");
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
fn stateful_operators_are_reported_as_rejected_until_stage_6() {
    let export = demo_export();
    // Aggregation with its parameters missing: the missing parameters, and that it keeps state.
    let source = replaced(
        "  input.metrics -> metrics_policy\n",
        "  input.metrics -> rollup\n\n  node rollup = aggregate {\n    -> metrics_policy\n  }\n",
    );
    let (outcome, compiled) = compile(&source, Some(&export));
    assert!(outcome.ok && compiled.is_some(), "{:#?}", outcome.diagnostics);
    let stateful = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("keeps state between batches"))
        .collect::<Vec<_>>();
    assert_eq!(stateful.len(), 1, "{:#?}", outcome.diagnostics);
    assert_eq!(stateful[0].severity, Severity::Error);
    assert_eq!(stateful[0].range.line, 6);
    assert!(stateful[0].message.contains("until stage 6"));
    // Stateless operators are not reported.
    let (outcome, _) = compile(DEFAULT_SOURCE, Some(&export));
    assert!(!outcome
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("keeps state")));
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
