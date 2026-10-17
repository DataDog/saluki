use saluki_metric_plan::canonical::ResolvedValues;

use super::*;
use crate::{
    compile::{compile, default_program_source, CompileOutcome},
    preview::{ActivePlan, CompileContext},
    scenario::{edits::append_to_metrics, tests::named_fixture},
};

/// Compiles `source` against the plan crate's default program, detached, as the editor does with ADP at hand.
fn compile_against_default(source: &str) -> CompileOutcome {
    let (plan, export) = named_fixture();
    let labels = RecipientLabels::default();
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: Some(ActivePlan {
            plan: &plan,
            hash: plan.hash(ResolvedValues::Include).unwrap(),
            generation: Some(3),
            artifact_revision: None,
        }),
        others: &[],
        revision: 7,
        labels: &labels,
        in_effect: None,
    };
    compile(source, Some(&context)).0
}

/// Puts a recipe's line into the default program, as its text says to: at the end of the stream every destination
/// reads, between the lines the recipe shows around it.
fn with_recipe(source: &str, recipe: &RecipeEntry) -> String {
    assert!(source.contains(&format!("{}\n", recipe.above)), "{}", source);
    let edited = append_to_metrics(source, recipe.stage).unwrap();
    assert!(
        edited.contains(&format!("{}\n{}\n", recipe.above, recipe.line)),
        "{}",
        edited
    );
    let line = edited.find(&format!("{}\n", recipe.line)).unwrap();
    assert!(edited[line..].contains(&format!("\n{}\n", recipe.below)), "{}", edited);
    edited
}

/// Returns the binding names an example's text reads: each `config.<name>`, its name the identifiers and dots after
/// `config.`.
fn bindings_read(text: &str) -> Vec<&str> {
    text.match_indices("config.")
        .filter(|(at, _)| {
            !text[..*at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        })
        .map(|(at, found)| {
            let rest = &text[at + found.len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(rest.len());
            rest[..end].trim_end_matches('.')
        })
        .collect()
}

/// Parses an example, wrapped in a program, or panics with the diagnostics.
fn parse_example(what: &str, example: &str) -> saluki_metric_plan::ir::PolicyArtifact {
    let wrapper = example_wrapper(example);
    match parse_with(&wrapper, &ParseOptions::default()) {
        Ok(parsed) => parsed.artifact,
        Err(diagnostics) => panic!("{} does not parse: {:?}\n{}", what, diagnostics, wrapper),
    }
}

#[test]
fn the_binding_names_an_example_reads_are_found() {
    assert_eq!(
        bindings_read("name in config.host.tags and config.multi_region_failover.enabled."),
        ["host.tags", "multi_region_failover.enabled"]
    );
    assert!(bindings_read("tags.config.x").is_empty());
}

#[test]
fn every_operator_a_program_may_write_has_an_example_that_configures() {
    let reference = reference(None, &RecipientLabels::default(), None);
    for operator in &reference.operators {
        if !operator.authorable {
            assert!(operator.example.is_none(), "{} is the host's", operator.name);
            continue;
        }
        let example = operator
            .example
            .unwrap_or_else(|| panic!("{} has no example", operator.name));
        parse_example(operator.name, example);
        assert!(
            example_ports(operator.name, example).is_some(),
            "{}'s example does not configure:\n{}",
            operator.name,
            example
        );
        assert!(!operator.inputs.is_empty(), "{} has no input port", operator.name);
    }
    // The host's operators come last.
    let first_host = reference
        .operators
        .iter()
        .position(|operator| !operator.authorable)
        .unwrap();
    assert!(reference.operators[first_host..]
        .iter()
        .all(|operator| !operator.authorable));
    assert!(reference
        .operators
        .iter()
        .any(|operator| operator.name == names::SOURCE && !operator.authorable));
}

#[test]
fn the_ports_are_the_ones_the_operators_give_their_examples() {
    let reference = reference(None, &RecipientLabels::default(), None);
    let ports = |name: &str| {
        let operator = reference
            .operators
            .iter()
            .find(|operator| operator.name == name)
            .unwrap();
        (operator.inputs.clone(), operator.outputs.clone())
    };
    assert_eq!(
        ports(names::SPLIT),
        (vec!["in".into()], vec!["true".into(), "false".into()])
    );
    assert_eq!(
        ports(names::TIMESTAMP_SPLIT),
        (vec!["in".into()], vec!["default".into(), "passthrough".into()])
    );
    assert_eq!(ports(names::TEE), (vec!["in".into()], vec!["0".into(), "1".into()]));
    assert_eq!(ports(names::MERGE), (vec!["0".into(), "1".into()], vec!["out".into()]));
    assert_eq!(ports(names::DROP), (vec!["in".into()], Vec::<String>::new()));
}

#[test]
fn parameters_show_their_types_positions_and_defaults_from_the_registry() {
    let reference = reference(None, &RecipientLabels::default(), None);
    let operator = |name: &str| {
        reference
            .operators
            .iter()
            .find(|operator| operator.name == name)
            .unwrap()
    };
    let aggregate = operator(names::AGGREGATE);
    let param = |name: &str| aggregate.params.iter().find(|param| param.name == name).unwrap();
    assert_eq!(param("window").ty, "duration: 10s, 5m");
    assert!(param("window").bindable && param("window").required);
    assert_eq!(param("context_limit").ty, "integer ≥ 0");
    assert_eq!(
        param("histogram_aggregates").ty,
        "[\"count\" | \"sum\" | \"min\" | \"max\" | \"avg\" | \"median\", ...]"
    );
    let split = operator(names::SPLIT);
    assert_eq!(split.params[0].ty, "predicate");
    assert!(!split.params[0].bindable && split.params[0].positional);
    assert!(operator(names::TIMESTAMP_SPLIT).params[0].positional);

    // Every parameter the registry gives a default shows it, as the syntax writes the value.
    let registry = OperatorRegistry::native();
    for def in registry.iter() {
        let entry = operator(def.name());
        for spec in def.params() {
            let param = entry.params.iter().find(|param| param.name == spec.name).unwrap();
            assert_eq!(
                param.default.is_some(),
                spec.default.is_some(),
                "{}.{}",
                def.name(),
                spec.name
            );
            assert_eq!(param.positional, spec.positional, "{}.{}", def.name(), spec.name);
        }
    }
}

#[test]
fn defaults_are_written_as_the_syntax_writes_values() {
    let durations = ValueType::List {
        element: Box::new(ValueType::Duration),
    };
    assert_eq!(syntax_value(&serde_json::json!(["5m", "10s"]), &durations), "[5m, 10s]");
    assert_eq!(syntax_value(&serde_json::json!("5m"), &ValueType::Duration), "5m");
    assert_eq!(syntax_value(&serde_json::json!("max"), &ValueType::String), "\"max\"");
    assert_eq!(syntax_value(&serde_json::json!([95, 99]), &durations), "[95, 99]");
    assert_eq!(syntax_value(&serde_json::json!([]), &ValueType::String), "[]");
    assert_eq!(syntax_value(&serde_json::json!(false), &ValueType::Bool), "false");
}

#[test]
fn every_predicate_example_parses_as_a_where() {
    for (_, predicate) in PREDICATES {
        let example = format!("dogstatsd\n  | where {}\n  | emit primary", predicate);
        parse_example(predicate, &example);
        assert!(
            example_ports(names::SPLIT, &example).is_some(),
            "{:?} does not configure",
            predicate
        );
    }
}

#[test]
fn each_piece_of_shorthand_describes_the_same_graph_as_its_long_form() {
    let sorted = |artifact: saluki_metric_plan::ir::PolicyArtifact| {
        let mut nodes = artifact
            .nodes
            .iter()
            .map(|node| serde_json::to_string(node).unwrap())
            .collect::<Vec<_>>();
        nodes.sort();
        let mut edges = artifact
            .edges
            .iter()
            .map(|edge| format!("{} -> {}", edge.from, edge.to))
            .collect::<Vec<_>>();
        edges.sort();
        (nodes, edges)
    };
    for (form, sugar, core, _) in SUGAR {
        assert_eq!(
            sorted(parse_example(form, sugar)),
            sorted(parse_example(form, core)),
            "{}:\n{}\n\n{}",
            form,
            sugar,
            core
        );
    }
}

#[test]
fn the_example_program_parses_and_its_nodes_are_listed() {
    if let Err(diagnostics) = parse_with(
        EXAMPLE_PROGRAM,
        &ParseOptions {
            base: Some(named_fixture().1.artifact.artifact.base.clone()),
        },
    ) {
        panic!("{:?}", diagnostics);
    }
    let nodes = reference(None, &RecipientLabels::default(), None).example_nodes;
    // `let dsd = ... | where ... | aggregate(...)`: the last stage is `dsd`, the one before it `dsd_split`.
    for (id, op) in [("dsd", names::AGGREGATE), ("dsd_split", names::SPLIT)] {
        assert!(
            nodes.iter().any(|node| node.id == id && node.op == op),
            "{} {} in {:?}",
            id,
            op,
            nodes
        );
    }
    // `dsd` is used twice, so a tee follows it.
    assert!(
        nodes
            .iter()
            .any(|node| node.id == "dsd_fanout" && node.op == names::TEE),
        "{:?}",
        nodes
    );
}

#[test]
fn every_recipe_is_one_line_that_compiles_into_the_default_program_with_no_finding() {
    let (_, export) = named_fixture();
    let source = default_program_source(&export, &RecipientLabels::default());
    for recipe in reference(Some(&export), &RecipientLabels::default(), None).recipes {
        assert!(!recipe.line.contains('\n'), "{}", recipe.line);
        let edited = with_recipe(&source, &recipe);
        assert!(edited.contains(&format!("\n{}\n", recipe.line)), "{}", edited);
        let outcome = compile_against_default(&edited);
        assert!(
            outcome.ok && outcome.errors == 0 && outcome.warnings == 0 && outcome.preview.valid,
            "{}: {:?}\n{:?}\n{}",
            recipe.title,
            outcome.diagnostics,
            outcome.preview,
            edited
        );
        assert!(!outcome.preview.unchanged, "{} changes nothing", recipe.title);
    }
}

#[test]
fn every_binding_an_example_reads_is_one_a_program_may_bind() {
    let (_, export) = named_fixture();
    let host = reference(Some(&export), &RecipientLabels::default(), None)
        .host
        .unwrap();
    let texts = PREDICATES
        .iter()
        .map(|entry| entry.1)
        .chain(OPERATOR_EXAMPLES.iter().map(|entry| entry.1))
        .chain(RECIPES.iter().map(|entry| entry.2))
        .chain(SUGAR.iter().flat_map(|entry| [entry.1, entry.2]))
        .chain([EXAMPLE_PROGRAM]);
    let mut read = 0;
    for text in texts {
        for name in bindings_read(text) {
            read += 1;
            let binding = host
                .bindings
                .iter()
                .find(|binding| binding.name == name)
                .unwrap_or_else(|| panic!("{} is not a binding", name));
            assert!(binding.bindable, "{} is a policy setting", name);
        }
    }
    assert!(read > 0);
}

#[test]
fn the_host_side_lists_the_inputs_the_directory_and_which_bindings_a_program_may_read() {
    let (_, export) = named_fixture();
    let labels = RecipientLabels::new(vec![("http://127.0.0.1:17101".to_string(), "Primary".to_string())]);
    let reference = reference(Some(&export), &labels, Some("unused".to_string()));
    assert_eq!(reference.host_missing, None);
    let host = reference.host.unwrap();
    assert_eq!(host.scope, "metrics");
    assert!(
        host.inputs.iter().any(|input| input == "dogstatsd"),
        "{:?}",
        host.inputs
    );
    assert!(host.detached && host.single_aggregation);
    let primary = host
        .recipients
        .iter()
        .find(|recipient| recipient.name == "primary")
        .unwrap();
    assert!(primary.endpoint.starts_with("http"), "{:?}", primary);
    // Policy settings are listed, but a detached program writes them as literals.
    assert!(host
        .bindings
        .iter()
        .any(|binding| binding.class == BindingClass::Policy && !binding.bindable));
    assert!(host
        .bindings
        .iter()
        .any(|binding| binding.class != BindingClass::Policy && binding.bindable));

    let without = super::reference(None, &labels, Some("ADP is not running.".to_string()));
    assert!(without.host.is_none());
    assert_eq!(without.host_missing.as_deref(), Some("ADP is not running."));
}

#[test]
fn the_rules_the_syntax_meets_are_the_structural_ones() {
    let rules = reference(None, &RecipientLabels::default(), None).rules;
    let met = rules
        .iter()
        .filter(|rule| rule.met_by_syntax)
        .map(|rule| rule.name)
        .collect::<Vec<_>>();
    assert_eq!(
        met,
        [
            Rule::OneEdgePerPort.as_str(),
            Rule::PathsEndInEmitOrDrop.as_str(),
            Rule::Acyclic.as_str()
        ]
    );
}
