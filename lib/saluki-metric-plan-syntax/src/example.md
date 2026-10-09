```rust
use saluki_metric_plan_syntax::{parse, print, PrintOptions};

let source = r#"policy post_enrich {
  base "sha256:1111111111111111111111111111111111111111111111111111111111111111"
  outputs {
    dd_metrics_encode = "sha256:2222222222222222222222222222222222222222222222222222222222222222",
    metrics_routing_encode_0 = "sha256:3333333333333333333333333333333333333333333333333333333333333333",
  }

  input.metrics -> drop_debug

  node drop_debug = split(name starts_with "demo.debug.") {
    true  -> drop
    false -> metrics_policy
  }

  node metrics_policy = tee {
    -> emit dd_metrics_encode
    -> metrics_routing_filter_0
  }

  node metrics_routing_filter_0 = split(intake_name prefix_in ["demo.checkout."] or tag("team") == "platform") {
    true  -> emit metrics_routing_encode_0
    false -> drop
  }
}
"#;

let parsed = parse(source).expect("the example parses");
let artifact = &parsed.artifact;
assert_eq!(artifact.scope.as_str(), "post_enrich");

// Inline terminals are nodes named after their parent and port, as lowering names them.
let ids = artifact.nodes.iter().map(|node| node.id.as_str()).collect::<Vec<_>>();
assert_eq!(
    ids,
    [
        "drop_debug",
        "drop_debug_true",
        "metrics_policy",
        "metrics_policy_0",
        "metrics_routing_filter_0",
        "metrics_routing_filter_0_true",
        "metrics_routing_filter_0_false",
    ]
);

// Spans lead from the artifact back to the text.
let span = parsed.spans.param("metrics_routing_filter_0", "predicate").unwrap();
assert_eq!(
    &source[span.range()],
    r#"intake_name prefix_in ["demo.checkout."] or tag("team") == "platform""#
);

// This source is already in printed form, so printing gives it back.
assert_eq!(print(artifact, &PrintOptions::default()), source);
```
