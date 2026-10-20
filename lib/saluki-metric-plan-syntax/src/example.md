```rust
use saluki_metric_plan_syntax::{parse, print, PrintOptions};

let source = r#"policy {
  let metrics_in = (dogstatsd, otlp) | where not name starts_with "demo.debug."

  metrics_in | emit primary

  metrics_in | where intake_name starts_with "demo.checkout." or tags.team == "platform" | emit "https://b.example.com"
}
"#;

let parsed = parse(source).expect("the example parses");
let artifact = &parsed.artifact;
assert_eq!(artifact.nodes.len(), 8);

// Nodes take IDs derived from where they are written: the tuple's merge from the `let`, the filter's dropped metrics
// from the filter, the `tee` that copies `metrics_in` to its two uses from the stream, and each use from the `tee`.
let ids = artifact.nodes.iter().map(|node| node.id.as_str()).collect::<Vec<_>>();
assert_eq!(
    ids,
    [
        "metrics_in_in",
        "metrics_in",
        "metrics_in_fanout",
        "metrics_in_fanout_0",
        "metrics_in_fanout_1_split",
        "metrics_in_fanout_1_split_true",
        "metrics_in_fanout_1_split_false",
        "metrics_in_false",
    ]
);

// Spans lead from the artifact back to the text.
let span = parsed.spans.param("metrics_in_fanout_1_split", "predicate").unwrap();
assert_eq!(
    &source[span.range()],
    r#"intake_name starts_with "demo.checkout." or tags.team == "platform""#
);

// This source is already in printed form, so printing gives it back.
assert_eq!(print(artifact, &PrintOptions::default()), source);
```
