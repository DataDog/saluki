use saluki_metric_plan::canonical::ResolvedValues;

use super::*;
use crate::{
    compile::{compile, default_program_source, CompileOutcome},
    preview::{ActivePlan, CompileContext},
    scenario::tests::named_fixture,
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

/// Puts a recipe's node between enrichment and the policy `tee`, as its text says to.
fn with_recipe(source: &str, recipe: &RecipeEntry) -> String {
    let edge = format!("\n    -> {}\n", NEXT);
    assert!(source.contains(&edge), "{}", source);
    let id = recipe
        .source
        .strip_prefix("node ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap();
    let source = source.replacen(&edge, &format!("\n    -> {}\n", id), 1);
    let at = source.find(&format!("\n  node {} = ", NEXT)).unwrap() + 1;
    let node = format!("  {}\n\n", recipe.source.replace('\n', "\n  "));
    format!("{}{}{}", &source[..at], node, &source[at..])
}

/// Returns the binding names an example's text reads.
fn bindings_read(text: &str) -> Vec<&str> {
    text.split("config(\"")
        .skip(1)
        .map(|rest| &rest[..rest.find('"').unwrap()])
        .collect()
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
        let wrapper = example_wrapper(example);
        if let Err(diagnostics) = parse_with(&wrapper, &ParseOptions::default()) {
            panic!(
                "{}'s example does not parse: {:?}\n{}",
                operator.name, diagnostics, wrapper
            );
        }
        assert!(
            example_ports(example).is_some(),
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
fn parameter_types_are_written_as_the_syntax_writes_values() {
    let reference = reference(None, &RecipientLabels::default(), None);
    let aggregate = reference
        .operators
        .iter()
        .find(|operator| operator.name == names::AGGREGATE)
        .unwrap();
    let param = |name: &str| aggregate.params.iter().find(|param| param.name == name).unwrap();
    assert_eq!(param("window").ty, "duration: 10s, 5m");
    assert!(param("window").bindable && param("window").required);
    assert_eq!(param("context_limit").ty, "integer ≥ 0");
    assert_eq!(
        param("histogram_aggregates").ty,
        "[\"count\" | \"sum\" | \"min\" | \"max\" | \"avg\" | \"median\", ...]"
    );
    let split = reference
        .operators
        .iter()
        .find(|operator| operator.name == names::SPLIT)
        .unwrap();
    assert_eq!(split.params[0].ty, "predicate");
    assert!(!split.params[0].bindable);
}

#[test]
fn every_predicate_example_parses_as_a_split() {
    for entry in PREDICATES {
        let example = format!(
            "node test = split({}) {{\n  true  -> next\n  false -> drop\n}}",
            entry.1
        );
        let wrapper = example_wrapper(&example);
        if let Err(diagnostics) = parse_with(&wrapper, &ParseOptions::default()) {
            panic!("{:?} does not parse: {:?}", entry.1, diagnostics);
        }
        assert!(example_ports(&example).is_some(), "{:?} does not configure", entry.1);
    }
}

#[test]
fn the_example_program_parses() {
    if let Err(diagnostics) = parse_with(
        EXAMPLE_PROGRAM,
        &ParseOptions {
            base: Some(named_fixture().1.artifact.artifact.base.clone()),
        },
    ) {
        panic!("{:?}", diagnostics);
    }
}

#[test]
fn every_recipe_compiles_into_the_default_program_with_no_finding() {
    let (_, export) = named_fixture();
    let source = default_program_source(&export, &RecipientLabels::default());
    for recipe in reference(None, &RecipientLabels::default(), None).recipes {
        let edited = with_recipe(&source, &recipe);
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
        .chain([EXAMPLE_PROGRAM]);
    for text in texts {
        for name in bindings_read(text) {
            let binding = host
                .bindings
                .iter()
                .find(|binding| binding.name == name)
                .unwrap_or_else(|| panic!("{} is not a binding", name));
            assert!(binding.bindable, "{} is a policy setting", name);
        }
    }
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
