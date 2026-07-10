//! Error-handling tests: lexer, parser-structure, syntax/grammar, and runtime errors.
//!
//! The bulk of these were previously ~31 near-identical single-expression functions. The ones that differ only by
//! input string (and all assert the same thing) are consolidated into two case tables; the ones with distinct
//! assertions or non-empty setup (message checks, registered converters, custom accessors) remain individual tests.

use std::sync::Arc;

use super::{empty_path_resolver_map, parse_with_empty_maps, UnitFamily};
use crate::parser::ast::Field;
use crate::parser::Parser;
use crate::{CallbackMap, EnumMap, OttlParser, PathAccessor, PathResolver, PathResolverMap, Value};

// ============================================================================
// Parse-time errors (table-driven)
// ============================================================================

#[test]
fn parse_time_errors_are_reported() {
    // Every expression here is rejected during parsing (lexer, structural, or grammar error). These collapsed from
    // ~17 single-expression tests that each built empty maps and asserted `is_error().is_err()`.
    let cases = [
        // Lexer errors
        ("unclosed string literal", r#""hello"#),
        ("invalid hex in bytes literal", "0xGG == 0x00"),
        ("single-quoted string", "'single quote'"),
        // Parser-structure errors
        ("missing right operand", "1 + "),
        ("two operators in a row", "1 * / 2"),
        ("unclosed parenthesis", "(1 + 2"),
        ("extra closing parenthesis", "1 + 2)"),
        ("empty parentheses", "()"),
        ("unclosed index bracket", "path[0"),
        ("empty expression", ""),
        ("whitespace-only expression", "   \t  "),
        // Syntax / grammar errors
        ("double comparison operator", "1 < < 2"),
        ("where clause without editor", "where true"),
        ("unknown enum reference", "UNKNOWN_ENUM == 1"),
        ("chained comparison", "1 < 2 < 3"),
        ("path starting with a dot", ".invalid.path"),
        ("double dot in path", "path..field"),
    ];

    for (description, expression) in cases {
        let parser = parse_with_empty_maps(expression);
        assert!(
            parser.is_error().is_err(),
            "parser should report an error for {description}: `{expression}`"
        );
    }
}

#[test]
fn unsupported_expressions_never_execute_successfully() {
    // These may be rejected at parse time OR at execution time, but must never successfully produce a value.
    // Consolidates the former unknown-function / unknown-converter / string-times-int / bool-less-than tests, which
    // all shared the "if it parses, executing it must fail" shape.
    let cases = [
        ("unknown editor function", "unknownFunc()"),
        ("unknown converter", "UnknownConverter() == 1"),
        ("string multiplied by integer", r#""hello" * 2"#),
        ("less-than on booleans", "true < false"),
    ];

    for (description, expression) in cases {
        let parser = parse_with_empty_maps(expression);
        if parser.is_error().is_ok() {
            let result = parser.execute(&mut ());
            assert!(result.is_err(), "executing {description} (`{expression}`) should fail");
        }
    }
}

// ============================================================================
// Errors with distinct assertions or setup (kept individual)
// ============================================================================

#[test]
fn lexer_error_reports_invalid_character() {
    // The `@` character is not a valid token; the error message must name it, whether it appears amidst valid tokens
    // or in an expression made up entirely of invalid characters.
    for expression in ["1 + @ + 2", "@#$%^&"] {
        let parser = parse_with_empty_maps(expression);
        let err = parser.is_error();
        assert!(err.is_err(), "parser should report an error for `{expression}`");
        assert!(
            err.unwrap_err().to_string().contains('@'),
            "error for `{expression}` should mention the invalid character"
        );
    }
}

#[test]
fn parse_error_reports_missing_comma_in_function() {
    // Registers an editor so `func(...)` is a recognized call; the missing comma is what must fail parsing.
    let mut editors = CallbackMap::new();
    editors.insert(
        "func".to_string(),
        Arc::new(|_args: &mut dyn crate::Args| Ok(Value::Nil)),
    );
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "func(1 2)");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for missing comma"
    );
}

#[test]
fn well_formed_division_by_zero_fails_at_execution() {
    // Both forms are syntactically valid (so parsing succeeds) but must fail at execution. The integer form must
    // additionally surface a division-by-zero message.
    let int_parser = parse_with_empty_maps("10 / 0");
    assert!(int_parser.is_error().is_ok(), "Parsing should succeed");
    let int_result = int_parser.execute(&mut ());
    assert!(int_result.is_err(), "Execute should fail with division by zero error");
    let err_msg = int_result.unwrap_err().to_string();
    assert!(
        err_msg.to_lowercase().contains("division") || err_msg.to_lowercase().contains("zero"),
        "Error should mention division by zero: {}",
        err_msg
    );

    let float_parser = parse_with_empty_maps("10.0 / 0.0");
    assert!(float_parser.is_error().is_ok(), "Parsing should succeed");
    assert!(
        float_parser.execute(&mut ()).is_err(),
        "Execute should fail with division by zero error"
    );
}

#[test]
fn execution_fails_for_index_out_of_bounds() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Register a converter that returns a small list
    converters.insert(
        "GetList".to_string(),
        Arc::new(|_args: &mut dyn crate::Args| Ok(Value::List(vec![Value::Int(1), Value::Int(2)]))),
    );

    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "GetList()[999] == 1");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");

    let result = parser.execute(&mut ());
    assert!(result.is_err(), "Execute should fail with index out of bounds");
}

#[test]
fn execution_fails_for_missing_map_key() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Register a converter that returns a map
    converters.insert(
        "GetMap".to_string(),
        Arc::new(|_args: &mut dyn crate::Args| {
            let mut map = std::collections::HashMap::new();
            map.insert("key1".to_string(), Value::Int(1));
            Ok(Value::Map(map))
        }),
    );

    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"GetMap()["nonexistent"] == 1"#,
    );

    assert!(parser.is_error().is_ok(), "Parsing should succeed");

    let result = parser.execute(&mut ());
    assert!(result.is_err(), "Execute should fail for non-existent key");
}

#[test]
fn execution_fails_for_negated_string() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Register a converter that returns a string
    converters.insert(
        "GetString".to_string(),
        Arc::new(|_args: &mut dyn crate::Args| Ok(Value::string("hello"))),
    );

    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "-GetString()");

    if parser.is_error().is_ok() {
        let result = parser.execute(&mut ());
        assert!(result.is_err(), "Execute should fail when negating a string");
    }
}

/// PathAccessor that fails on get (for the `path_not_found` test)
#[derive(Debug)]
struct FailingPathAccessor;
impl PathAccessor<UnitFamily> for FailingPathAccessor {
    fn get<'a>(&self, _ctx: &(), _fields: &[Field]) -> crate::Result<Value> {
        Err("Path resolver failed".into())
    }
    fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
        Err("Path resolver failed".into())
    }
}

#[test]
fn execution_fails_when_path_accessor_get_fails() {
    // Reference to a path whose accessor fails on get: parsing succeeds (a resolver exists), execution surfaces the
    // accessor error.
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    let failing_resolver: PathResolver<UnitFamily> =
        Arc::new(|| Ok(Arc::new(FailingPathAccessor) as Arc<dyn PathAccessor<UnitFamily>>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("nonexistent.path".to_string(), failing_resolver);

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "nonexistent.path == 1");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");
    let result = parser.execute(&mut ());
    assert!(result.is_err(), "Execute should fail for non-existent path");
}

#[test]
fn parse_error_reports_missing_path_resolver() {
    // Parsing fails when a path in the expression has no entry in path_resolvers, and the message must name both the
    // missing-resolver condition and the offending path.
    let parser = parse_with_empty_maps("some.unknown.path == 1");

    let err = parser.is_error();
    assert!(err.is_err(), "Parsing should fail when path has no resolver");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("No PathResolver provided for path"),
        "Error should mention missing path resolver: {}",
        msg
    );
    assert!(
        msg.contains("some.unknown.path"),
        "Error should mention the path: {}",
        msg
    );
}
