```rust
use saluki_metric_plan_syntax::{parse, print, PrintOptions};

let source = r#"policy metrics {
  base "sha256:1111111111111111111111111111111111111111111111111111111111111111"

  let metrics_in = (dogstatsd, otlp) | where not name starts_with "demo.debug."

  metrics_in | fork as metrics_policy {
    emit primary
    where intake_name starts_with "demo.checkout." or tags.team == "platform" | emit "https://b.example.com"
  }
}
"#;

let parsed = parse(source).expect("the example parses");
let artifact = &parsed.artifact;
assert_eq!(artifact.scope.as_str(), "metrics");

// Nodes written without `as` take IDs derived from where they are written: the tuple's merge from the `let`, the
// filter's dropped metrics from the filter, and each branch of the fork from the fork.
let ids = artifact.nodes.iter().map(|node| node.id.as_str()).collect::<Vec<_>>();
assert_eq!(
    ids,
    [
        "metrics_in_in",
        "metrics_in",
        "metrics_policy",
        "metrics_policy_0",
        "metrics_policy_1_split",
        "metrics_policy_1_split_true",
        "metrics_policy_1_split_false",
        "metrics_in_false",
    ]
);

// Spans lead from the artifact back to the text.
let span = parsed.spans.param("metrics_policy_1_split", "predicate").unwrap();
assert_eq!(
    &source[span.range()],
    r#"intake_name starts_with "demo.checkout." or tags.team == "platform""#
);

// This source is already in printed form, so printing gives it back.
assert_eq!(print(artifact, &PrintOptions::default()), source);
```
