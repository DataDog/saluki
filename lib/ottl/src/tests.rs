//! Tests for the OTTL lexer and library

use std::sync::Arc;

use crate::lexer::{Lexer, Token};
use crate::parser::Parser;
use crate::{CallbackMap, IndexExpr, EnumMap, EvalContext, OttlParser, PathAccessor, PathResolver, PathResolverMap, Value};

// ============================================================================
// Helper functions
// ============================================================================

/// Local helper for test PathAccessors: apply indexes to a value (integrator responsibility in real code).
fn test_apply_indexes(value: crate::Value, indexes: &[IndexExpr]) -> crate::Result<crate::Value> {
    let mut current = value;
    for index in indexes {
        current = match (&current, index) {
            (crate::Value::List(list), IndexExpr::Int(i)) => list
                .get(*i)
                .cloned()
                .ok_or_else(|| -> crate::BoxError { format!("Index {} out of bounds", i).into() })?,
            (crate::Value::Map(map), IndexExpr::String(key)) => map
                .get(key)
                .cloned()
                .ok_or_else(|| -> crate::BoxError { format!("Key '{}' not found", key).into() })?,
            (crate::Value::String(s), IndexExpr::Int(i)) => s
                .chars()
                .nth(*i)
                .map(|c| Value::string(c.to_string()))
                .ok_or_else(|| -> crate::BoxError { format!("Index {} out of bounds", i).into() })?,
            _ => return Err(format!("Cannot index {:?} with {:?}", current, index).into()),
        };
    }
    Ok(current)
}

/// Helper to collect tokens from input, panics on lexer error
fn collect_tokens(input: &str) -> Vec<Token<'_>> {
    Lexer::collect_with_spans(input)
        .expect("Lexer error")
        .into_iter()
        .map(|(token, _span)| token)
        .collect()
}

// ============================================================================
// Lexer tests
// ============================================================================

#[test]
fn test_keywords() {
    let tokens = collect_tokens("where or and not true false nil");
    assert_eq!(
        tokens,
        vec![
            Token::Where,
            Token::Or,
            Token::And,
            Token::Not,
            Token::True,
            Token::False,
            Token::Nil,
        ]
    );
}

#[test]
fn test_comparison_operators() {
    let tokens = collect_tokens("== != < > <= >=");
    assert_eq!(
        tokens,
        vec![
            Token::Eq,
            Token::NotEq,
            Token::Less,
            Token::Greater,
            Token::LessEq,
            Token::GreaterEq,
        ]
    );
}

#[test]
fn test_arithmetic_operators() {
    let tokens = collect_tokens("+ - * /");
    assert_eq!(tokens, vec![Token::Plus, Token::Minus, Token::Multiply, Token::Divide,]);
}

#[test]
fn test_delimiters() {
    let tokens = collect_tokens("( ) [ ] { } , . : =");
    assert_eq!(
        tokens,
        vec![
            Token::LParen,
            Token::RParen,
            Token::LBracket,
            Token::RBracket,
            Token::LBrace,
            Token::RBrace,
            Token::Comma,
            Token::Dot,
            Token::Colon,
            Token::Assign,
        ]
    );
}

#[test]
fn test_string_literal() {
    let tokens = collect_tokens(r#""hello world""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""hello world""#)]);
}

#[test]
fn test_string_with_escape() {
    let tokens = collect_tokens(r#""hello \"world\"""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""hello \"world\"""#)]);
}

#[test]
fn test_int_literal() {
    // Note: Signs are now separate tokens, handled by the parser
    let tokens = collect_tokens("42 0");
    assert_eq!(tokens, vec![Token::IntLiteral("42"), Token::IntLiteral("0"),]);
}

#[test]
fn test_signed_int_literal() {
    // Signs are separate tokens
    let tokens = collect_tokens("-10 +5");
    assert_eq!(
        tokens,
        vec![
            Token::Minus,
            Token::IntLiteral("10"),
            Token::Plus,
            Token::IntLiteral("5"),
        ]
    );
}

#[test]
fn test_float_literal() {
    // Note: Signs are now separate tokens, handled by the parser
    let tokens = collect_tokens("6.18 .5");
    assert_eq!(tokens, vec![Token::FloatLiteral("6.18"), Token::FloatLiteral(".5"),]);
}

#[test]
fn test_signed_float_literal() {
    // Signs are separate tokens
    let tokens = collect_tokens("-2.0 +0.1");
    assert_eq!(
        tokens,
        vec![
            Token::Minus,
            Token::FloatLiteral("2.0"),
            Token::Plus,
            Token::FloatLiteral("0.1"),
        ]
    );
}

#[test]
fn test_bytes_literal() {
    let tokens = collect_tokens("0xDEADBEEF 0x00 0xabc123");
    assert_eq!(
        tokens,
        vec![
            Token::BytesLiteral("0xDEADBEEF"),
            Token::BytesLiteral("0x00"),
            Token::BytesLiteral("0xabc123"),
        ]
    );
}

#[test]
fn test_identifiers() {
    let tokens = collect_tokens("myVar MyConverter");
    assert_eq!(
        tokens,
        vec![Token::LowerIdent("myVar"), Token::UpperIdent("MyConverter"),]
    );
}

#[test]
fn test_editor_invocation() {
    let tokens = collect_tokens("set(x, \"value\")");
    assert_eq!(
        tokens,
        vec![
            Token::LowerIdent("set"),
            Token::LParen,
            Token::LowerIdent("x"),
            Token::Comma,
            Token::StringLiteral("\"value\""),
            Token::RParen,
        ]
    );
}

#[test]
fn test_converter_invocation() {
    let tokens = collect_tokens("Concat(a, b)[0]");
    assert_eq!(
        tokens,
        vec![
            Token::UpperIdent("Concat"),
            Token::LParen,
            Token::LowerIdent("a"),
            Token::Comma,
            Token::LowerIdent("b"),
            Token::RParen,
            Token::LBracket,
            Token::IntLiteral("0"),
            Token::RBracket,
        ]
    );
}

#[test]
fn test_path_expression() {
    let tokens = collect_tokens("resource.attributes[\"key\"]");
    assert_eq!(
        tokens,
        vec![
            Token::LowerIdent("resource"),
            Token::Dot,
            Token::LowerIdent("attributes"),
            Token::LBracket,
            Token::StringLiteral("\"key\""),
            Token::RBracket,
        ]
    );
}

#[test]
fn test_boolean_expression() {
    let tokens = collect_tokens("x == 1 and y > 2 or not z");
    assert_eq!(
        tokens,
        vec![
            Token::LowerIdent("x"),
            Token::Eq,
            Token::IntLiteral("1"),
            Token::And,
            Token::LowerIdent("y"),
            Token::Greater,
            Token::IntLiteral("2"),
            Token::Or,
            Token::Not,
            Token::LowerIdent("z"),
        ]
    );
}

#[test]
fn test_math_expression() {
    let tokens = collect_tokens("10 + 20 * 3");
    assert_eq!(
        tokens,
        vec![
            Token::IntLiteral("10"),
            Token::Plus,
            Token::IntLiteral("20"),
            Token::Multiply,
            Token::IntLiteral("3"),
        ]
    );
}

#[test]
fn test_full_statement() {
    let tokens = collect_tokens(r#"set(attributes["key"], "value") where status == 200"#);
    assert_eq!(
        tokens,
        vec![
            Token::LowerIdent("set"),
            Token::LParen,
            Token::LowerIdent("attributes"),
            Token::LBracket,
            Token::StringLiteral("\"key\""),
            Token::RBracket,
            Token::Comma,
            Token::StringLiteral("\"value\""),
            Token::RParen,
            Token::Where,
            Token::LowerIdent("status"),
            Token::Eq,
            Token::IntLiteral("200"),
        ]
    );
}

#[test]
fn test_named_args() {
    let tokens = collect_tokens("merge(target = x, source = y)");
    assert_eq!(
        tokens,
        vec![
            Token::LowerIdent("merge"),
            Token::LParen,
            Token::LowerIdent("target"),
            Token::Assign,
            Token::LowerIdent("x"),
            Token::Comma,
            Token::LowerIdent("source"),
            Token::Assign,
            Token::LowerIdent("y"),
            Token::RParen,
        ]
    );
}

#[test]
fn test_map_literal() {
    let tokens = collect_tokens(r#"{"key": "value", "count": 42}"#);
    assert_eq!(
        tokens,
        vec![
            Token::LBrace,
            Token::StringLiteral("\"key\""),
            Token::Colon,
            Token::StringLiteral("\"value\""),
            Token::Comma,
            Token::StringLiteral("\"count\""),
            Token::Colon,
            Token::IntLiteral("42"),
            Token::RBrace,
        ]
    );
}

#[test]
fn test_list_literal() {
    let tokens = collect_tokens("[1, 2, 3]");
    assert_eq!(
        tokens,
        vec![
            Token::LBracket,
            Token::IntLiteral("1"),
            Token::Comma,
            Token::IntLiteral("2"),
            Token::Comma,
            Token::IntLiteral("3"),
            Token::RBracket,
        ]
    );
}

#[test]
fn test_enum() {
    let tokens = collect_tokens("SPAN_KIND_SERVER STATUS_OK");
    assert_eq!(
        tokens,
        vec![Token::UpperIdent("SPAN_KIND_SERVER"), Token::UpperIdent("STATUS_OK"),]
    );
}

#[test]
fn test_whitespace_handling() {
    let tokens = collect_tokens("  set  (  x  ,  y  )  ");
    assert_eq!(
        tokens,
        vec![
            Token::LowerIdent("set"),
            Token::LParen,
            Token::LowerIdent("x"),
            Token::Comma,
            Token::LowerIdent("y"),
            Token::RParen,
        ]
    );
}

// ============================================================================
// Parser tests
// ============================================================================

use crate::Argument;

#[test]
fn test_value_equality() {
    assert_eq!(Value::Bool(true), Value::Bool(true));
    assert_eq!(Value::Int(42), Value::Int(42));
    assert_eq!(Value::Float(6.18), Value::Float(6.18));
    assert_eq!(Value::String("hello".into()), Value::String("hello".into()));
    assert_eq!(Value::Nil, Value::Nil);

    assert_ne!(Value::Bool(true), Value::Bool(false));
    assert_ne!(Value::Int(1), Value::Int(2));
    assert_ne!(Value::Bool(true), Value::Int(1));
}

#[test]
fn test_argument_access() {
    let pos = Argument::Positional(Value::Int(42));
    match &pos {
        Argument::Positional(v) => assert_eq!(v, &Value::Int(42)),
        _ => panic!("expected Positional"),
    }

    let named = Argument::Named {
        name: "foo".into(),
        value: Value::String("bar".into()),
    };
    match &named {
        Argument::Named { name, value } => {
            assert_eq!(name, "foo");
            assert_eq!(value, &Value::String("bar".into()));
        }
        _ => panic!("expected Named"),
    }
}

// ============================================================================
// Parser integration tests
// ============================================================================

/// Stub PathAccessor that does nothing (for testing purposes).
#[derive(Debug)]
struct StubPathAccessor;

impl PathAccessor for StubPathAccessor {
    fn get(&self, _ctx: &EvalContext, _path: &str) -> crate::Result<Value> {
        Err("StubPathAccessor: get not implemented".into())
    }

    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }

    fn set(&self, _ctx: &mut EvalContext, _path: &str, _value: &Value) -> crate::Result<()> {
        Err("StubPathAccessor: set not implemented".into())
    }
}

/// Create an empty path resolver map (for expressions with no paths)
fn empty_path_resolver_map() -> PathResolverMap {
    PathResolverMap::new()
}

/// Create a path resolver map with stub accessor for each given path.
fn stub_path_resolver_for(paths: &[&str]) -> PathResolverMap {
    let stub: PathResolver =
        Arc::new(|| -> crate::Result<Arc<dyn PathAccessor + Send + Sync>> { Ok(Arc::new(StubPathAccessor)) });
    paths.iter().map(|&p| (p.to_string(), stub.clone())).collect()
}

/// Create a stub EvalContext
fn stub_context() -> EvalContext {
    Box::new(())
}

#[test]
fn test_stub_path_resolver_for_execute_fails() {
    // stub_path_resolver_for provides resolvers so parsing succeeds;
    // execute fails because StubPathAccessor::get returns Err
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = stub_path_resolver_for(&["stub.path"]);

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "stub.path == 1");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");
    let result = parser.execute(&mut stub_context());
    assert!(
        result.is_err(),
        "Execute should fail: stub accessor returns Err from get"
    );
}

#[test]
fn test_parser_math_expression() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "-1+   2*10 - 10/5 - (1+3*2)",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(10));
}

#[test]
fn test_parser_bool_expression_with_math() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "false or not (2 < (1 + 2)) or (0xDEADBEEF == nil) or (1 != 2) or (2 >= 1.5) and (true) and \"banana 🎉\" > \"apple\"",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    // false or (2 < 3) = false or true = true
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Bool(true));
}

/// PathAccessor that returns predefined values for specific paths
#[derive(Debug)]
struct MockPathAccessor {
    bool_value: Value,
    int_value: Value,
}

impl PathAccessor for MockPathAccessor {
    fn get(&self, _ctx: &EvalContext, path: &str) -> crate::Result<Value> {
        match path {
            "my.bool.value" => Ok(self.bool_value.clone()),
            "my.int.value" => Ok(self.int_value.clone()),
            _ => Err(format!("Unknown path: {}", path).into()),
        }
    }

    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }

    fn set(&self, _ctx: &mut EvalContext, _path: &str, _value: &Value) -> crate::Result<()> {
        Err("MockPathAccessor: set not implemented".into())
    }
}

/// Create a PathResolverMap with MockPathAccessor for my.bool.value and my.int.value
fn mock_path_resolver_map(bool_value: bool, int_value: i64) -> PathResolverMap {
    let accessor = Arc::new(MockPathAccessor {
        bool_value: Value::Bool(bool_value),
        int_value: Value::Int(int_value),
    });
    let resolver: PathResolver = Arc::new(move || Ok(accessor.clone()));
    let mut m = PathResolverMap::new();
    m.insert("my.bool.value".to_string(), resolver.clone());
    m.insert("my.int.value".to_string(), resolver);
    m
}

#[test]
fn test_parser_bool_expression_with_paths() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    // Create resolver map for my.bool.value and my.int.value
    let path_resolvers = mock_path_resolver_map(false, 2);
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "my.bool.value or (my.int.value < (1 + 2))",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    // my.bool.value = false
    // my.int.value = 2
    // 1 + 2 = 3
    // my.int.value < 3 = 2 < 3 = true
    // false or true = true
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Bool(true));
}

#[test]
fn test_parser_math_with_converters() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let enums = EnumMap::new();

    // Register Sum converter: Sum(a: int, b: int) -> int { a + b }
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("Sum: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("Sum: second argument must be int".into()),
            };
            Ok(Value::Int(a + b))
        }),
    );

    let path_resolvers = mock_path_resolver_map(false, 0);
    let mut ctx = stub_context();

    // Expression: Sum(1, 2) + 10 * Sum(-1, 1)
    // Sum(1, 2) = 1 + 2 = 3
    // Sum(-1, 1) = -1 + 1 = 0
    // 10 * 0 = 0
    // 3 + 0 = 3
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Sum(1, 2) + 10 * Sum(-1, 1)",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(3));

    // Test: Sum(1,2) * Sum(2,4) = 3 * 6 = 18
    let parser2 = Parser::new(&editors, &converters, &enums, &path_resolvers, "Sum(1,2) * Sum(2,4)");

    if let Err(e) = parser2.is_error() {
        panic!("Parser error: {}", e);
    }

    let result2 = parser2.execute(&mut ctx);
    assert!(result2.is_ok(), "Execution should succeed: {:?}", result2);
    assert_eq!(result2.unwrap(), Value::Int(18));
}

#[test]
fn test_parser_math_with_enums() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let mut enums = EnumMap::new();

    // Register enum values
    enums.insert("MY_INT_VALUE1".to_string(), 1);
    enums.insert("MY_INT_VALUE200".to_string(), 200);
    enums.insert("MY_INT_VALUE199".to_string(), 199);

    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Expression: MY_INT_VALUE200 - (MY_INT_VALUE1 + MY_INT_VALUE199)
    // 200 - (1 + 199) = 200 - 200 = 0
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "MY_INT_VALUE200 - (MY_INT_VALUE1 + MY_INT_VALUE199)",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(0));

    // Test with unary minus before enum
    // Expression: -MY_INT_VALUE1 + MY_INT_VALUE200
    // -1 + 200 = 199
    let parser2 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "-MY_INT_VALUE1 + MY_INT_VALUE200",
    );

    if let Err(e) = parser2.is_error() {
        panic!("Parser error with unary minus: {}", e);
    }

    let result2 = parser2.execute(&mut ctx);
    assert!(
        result2.is_ok(),
        "Execution with unary minus should succeed: {:?}",
        result2
    );
    assert_eq!(result2.unwrap(), Value::Int(199));
}

#[test]
fn test_parser_bool_expression_with_enums() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let mut enums = EnumMap::new();

    // Register enum values
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_NOT_FOUND".to_string(), 404);
    enums.insert("STATUS_ERROR".to_string(), 500);

    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Test 1: STATUS_OK < STATUS_NOT_FOUND (200 < 404 = true)
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "STATUS_OK < STATUS_NOT_FOUND",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Bool(true));

    // Test 2: STATUS_ERROR == 500 (500 == 500 = true)
    let parser2 = Parser::new(&editors, &converters, &enums, &path_resolvers, "STATUS_ERROR == 500");

    if let Err(e) = parser2.is_error() {
        panic!("Parser error: {}", e);
    }

    let result2 = parser2.execute(&mut ctx);
    assert!(result2.is_ok(), "Execution should succeed: {:?}", result2);
    assert_eq!(result2.unwrap(), Value::Bool(true));

    // Test 3: Complex boolean with enums
    // (STATUS_OK < STATUS_NOT_FOUND) and (STATUS_ERROR > 400)
    // (200 < 404) and (500 > 400) = true and true = true
    let parser3 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "(((STATUS_OK < STATUS_NOT_FOUND))) and (STATUS_ERROR > 400)",
    );

    if let Err(e) = parser3.is_error() {
        panic!("Parser error: {}", e);
    }

    let result3 = parser3.execute(&mut ctx);
    assert!(result3.is_ok(), "Execution should succeed: {:?}", result3);
    assert_eq!(result3.unwrap(), Value::Bool(true));
}

#[test]
fn test_parser_enums_as_function_args() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let mut enums = EnumMap::new();

    // Register enum values
    enums.insert("VALUE_10".to_string(), 10);
    enums.insert("VALUE_20".to_string(), 20);
    enums.insert("VALUE_5".to_string(), 5);

    // Register Sum converter: Sum(a: int, b: int) -> int { a + b }
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("Sum: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("Sum: second argument must be int".into()),
            };
            Ok(Value::Int(a + b))
        }),
    );

    // Register Multiply converter: Multiply(a: int, b: int) -> int { a * b }
    converters.insert(
        "Multiply".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("Multiply: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("Multiply: second argument must be int".into()),
            };
            Ok(Value::Int(a * b))
        }),
    );

    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Test 1: Sum(VALUE_10, VALUE_20) + 0 = 10 + 20 + 0 = 30
    // Note: We add "+ 0" to force parsing as math expression (not bool)
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Sum(VALUE_10, VALUE_20) + 0",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(30));

    // Test 2: Multiply(VALUE_5, VALUE_10) * 1 = 5 * 10 * 1 = 50
    let parser2 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Multiply(VALUE_5, VALUE_10) * 1",
    );

    if let Err(e) = parser2.is_error() {
        panic!("Parser error: {}", e);
    }

    let result2 = parser2.execute(&mut ctx);
    assert!(result2.is_ok(), "Execution should succeed: {:?}", result2);
    assert_eq!(result2.unwrap(), Value::Int(50));

    // Test 3: Nested - Sum(Multiply(VALUE_5, VALUE_10), VALUE_20) + 0 = (5*10) + 20 + 0 = 70
    let parser3 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Sum(Multiply(VALUE_5, VALUE_10), VALUE_20) + 0",
    );

    if let Err(e) = parser3.is_error() {
        panic!("Parser error: {}", e);
    }

    let result3 = parser3.execute(&mut ctx);
    assert!(result3.is_ok(), "Execution should succeed: {:?}", result3);
    assert_eq!(result3.unwrap(), Value::Int(70));
}

// ============================================================================
// Editor Statement Tests
// ============================================================================

use std::sync::Mutex;

/// Structure to capture editor call information
#[derive(Debug, Clone, Default)]
struct EditorCallCapture {
    called: bool,
    first_arg: Option<Value>,
    second_arg: Option<Value>,
}

/// PathAccessor that supports both get and set, with tracking
#[derive(Debug)]
struct TrackingPathAccessor {
    int_value: Value,
    status_code: Value,
    target_path: Value, // Dummy value for "target" path
    set_calls: Mutex<Vec<(String, Value)>>,
}

impl PathAccessor for TrackingPathAccessor {
    fn get(&self, _ctx: &EvalContext, path: &str) -> crate::Result<Value> {
        match path {
            "my.int.value" => Ok(self.int_value.clone()),
            "status_code" => Ok(self.status_code.clone()),
            "target" | "x" => Ok(self.target_path.clone()), // For editor's first argument
            _ => Err(format!("Unknown path: {}", path).into()),
        }
    }

    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }

    fn set(&self, _ctx: &mut EvalContext, path: &str, value: &Value) -> crate::Result<()> {
        self.set_calls.lock().unwrap().push((path.to_string(), value.clone()));
        Ok(())
    }
}

/// Create a PathResolverMap with tracking accessor for target, my.int.value, status_code
fn tracking_path_resolver_map(int_value: i64, status_code: i64) -> (PathResolverMap, Arc<TrackingPathAccessor>) {
    let accessor = Arc::new(TrackingPathAccessor {
        int_value: Value::Int(int_value),
        status_code: Value::Int(status_code),
        target_path: Value::Nil, // Dummy value for target path
        set_calls: Mutex::new(Vec::new()),
    });
    let accessor_clone = accessor.clone();
    let resolver: PathResolver =
        Arc::new(move || -> crate::Result<Arc<dyn PathAccessor + Send + Sync>> { Ok(accessor_clone.clone()) });
    let mut m = PathResolverMap::new();
    m.insert("target".to_string(), resolver.clone());
    m.insert("x".to_string(), resolver.clone());
    m.insert("my.int.value".to_string(), resolver.clone());
    m.insert("status_code".to_string(), resolver);
    (m, accessor)
}

#[test]
fn test_editor_executes_when_condition_true() {
    // Setup: condition will be TRUE
    // my.int.value = 50 (> 0)
    // status_code = 200 (== STATUS_OK)

    let call_capture = Arc::new(Mutex::new(EditorCallCapture::default()));
    let capture_clone = call_capture.clone();

    let mut editors = CallbackMap::new();
    editors.insert(
        "set".to_string(),
        Arc::new(move |args: &mut dyn crate::Args| {
            let mut capture = capture_clone.lock().unwrap();
            capture.called = true;
            capture.first_arg = args.get(0).ok();
            capture.second_arg = args.get(1).ok();
            Ok(Value::Nil)
        }),
    );

    let mut converters = CallbackMap::new();
    // Sum converter: Sum(a, b) -> a + b
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: first argument must be numeric".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: second argument must be numeric".into()),
            };
            Ok(Value::Float(a + b))
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_WEIGHT".to_string(), 100);
    enums.insert("STATUS_OK".to_string(), 200);

    // my.int.value = 50, status_code = 200 (matches STATUS_OK)
    let (path_resolvers, _accessor) = tracking_path_resolver_map(50, 200);
    let mut ctx = stub_context();

    // Expression:
    // set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK
    // Sum(100, 50) * 1.5 = 150 * 1.5 = 225.0
    // Condition: 50 > 0 (true) and 200 == 200 (true) -> true
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Nil); // Editor returns Nil

    // Verify editor was called
    let capture = call_capture.lock().unwrap();
    assert!(capture.called, "Editor 'set' should have been called");

    // Verify first argument (path "target" resolves to Nil in our mock)
    assert_eq!(
        capture.first_arg,
        Some(Value::Nil),
        "First argument should be the resolved path value (Nil)"
    );

    // Verify second argument (computed value = 225.0)
    // Sum(STATUS_WEIGHT=100, my.int.value=50) * 1.5 = 150 * 1.5 = 225.0
    assert_eq!(
        capture.second_arg,
        Some(Value::Float(225.0)),
        "Second argument should be Sum(100, 50) * 1.5 = 225.0"
    );
}

#[test]
fn test_editor_not_executed_when_condition_false() {
    // Setup: condition will be FALSE
    // my.int.value = -10 (NOT > 0)
    // status_code = 200 (== STATUS_OK, but first part is false)

    let call_capture = Arc::new(Mutex::new(EditorCallCapture::default()));
    let capture_clone = call_capture.clone();

    let mut editors = CallbackMap::new();
    editors.insert(
        "set".to_string(),
        Arc::new(move |args: &mut dyn crate::Args| {
            let mut capture = capture_clone.lock().unwrap();
            capture.called = true;
            capture.first_arg = args.get(0).ok();
            capture.second_arg = args.get(1).ok();
            Ok(Value::Nil)
        }),
    );

    let mut converters = CallbackMap::new();
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: first argument must be numeric".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: second argument must be numeric".into()),
            };
            Ok(Value::Float(a + b))
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_WEIGHT".to_string(), 100);
    enums.insert("STATUS_OK".to_string(), 200);

    // my.int.value = -10 (negative!), status_code = 200
    let (path_resolvers, _accessor) = tracking_path_resolver_map(-10, 200);
    let mut ctx = stub_context();

    // Expression:
    // set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK
    // Condition: -10 > 0 (FALSE) and 200 == 200 (true) -> false (short-circuit)
    // Editor should NOT be called
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Nil); // Still returns Nil

    // Verify editor was NOT called
    let capture = call_capture.lock().unwrap();
    assert!(
        !capture.called,
        "Editor 'set' should NOT have been called when condition is false"
    );
    assert!(capture.first_arg.is_none(), "No arguments should be captured");
    assert!(capture.second_arg.is_none(), "No arguments should be captured");
}

#[test]
fn test_editor_set_list_of_maps() {
    // Test: set(x, [{"id": 1, "value": Double(5.0)}, {"id": 2, "value": STATUS_OK}, {"id": 3, "value": my.int.value}])
    // Expected result: x = [{"id": 1, "value": 10.0}, {"id": 2, "value": 200}, {"id": 3, "value": 73}]

    let call_capture = Arc::new(Mutex::new(EditorCallCapture::default()));
    let capture_clone = call_capture.clone();

    let mut editors = CallbackMap::new();
    editors.insert(
        "set".to_string(),
        Arc::new(move |args: &mut dyn crate::Args| {
            let mut capture = capture_clone.lock().unwrap();
            capture.called = true;
            capture.first_arg = args.get(0).ok();
            capture.second_arg = args.get(1).ok();
            Ok(Value::Nil)
        }),
    );

    let mut converters = CallbackMap::new();
    // Double converter: Double(x) -> x * 2
    converters.insert(
        "Double".to_string(),
        Arc::new(|args: &mut dyn crate::Args| match args.get(0)? {
            Value::Int(v) => Ok(Value::Int(v * 2)),
            Value::Float(v) => Ok(Value::Float(v * 2.0)),
            _ => Err("Double: argument must be numeric".into()),
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);

    // my.int.value = 73
    let (path_resolvers, _accessor) = tracking_path_resolver_map(73, 200);
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "set(x, [{\"id\": 1, \"value\": Double(5.0) ,}, {\"id\": 2, \"value\": STATUS_OK}, {\"id\": 3, \"value\": my.int.value}])",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Nil);

    // Verify editor was called
    let capture = call_capture.lock().unwrap();
    assert!(capture.called, "Editor 'set' should have been called");

    // Verify first argument (path "x" resolves to Nil in our mock)
    assert_eq!(
        capture.first_arg,
        Some(Value::Nil),
        "First argument should be the resolved path value"
    );

    // Verify second argument - list of maps with computed values
    // Expected: [{"id": 1, "value": 10.0}, {"id": 2, "value": 200}, {"id": 3, "value": 73}]
    use std::collections::HashMap;

    let mut map1 = HashMap::new();
    map1.insert("id".to_string(), Value::Int(1));
    map1.insert("value".to_string(), Value::Float(10.0));

    let mut map2 = HashMap::new();
    map2.insert("id".to_string(), Value::Int(2));
    map2.insert("value".to_string(), Value::Int(200));

    let mut map3 = HashMap::new();
    map3.insert("id".to_string(), Value::Int(3));
    map3.insert("value".to_string(), Value::Int(73));

    let expected_list = Value::List(vec![Value::Map(map1), Value::Map(map2), Value::Map(map3)]);

    assert_eq!(
        capture.second_arg,
        Some(expected_list),
        "Second argument should be the list of maps with computed values"
    );
}

// ============================================================================
// Path Expressions Tests
// ============================================================================

/// PathAccessor that supports multi-level paths, index access, and key access
#[derive(Debug)]
struct PathExprAccessor {
    // resource.attributes.status = 200
    // resource.count = 10
    resource_status: Value,
    resource_count: Value,
    // items[0] = 5, items[1] = 3
    items: Value,
    // data["key"] = 100, data["multiplier"] = 2
    data: Value,
}

impl PathAccessor for PathExprAccessor {
    fn get(&self, _ctx: &EvalContext, path: &str) -> crate::Result<Value> {
        match path {
            "resource.attributes.status" => Ok(self.resource_status.clone()),
            "resource.count" => Ok(self.resource_count.clone()),
            "items" => Ok(self.items.clone()),
            "data" => Ok(self.data.clone()),
            _ => Err(format!("Unknown path: {}", path).into()),
        }
    }

    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }

    fn set(&self, _ctx: &mut EvalContext, _path: &str, _value: &Value) -> crate::Result<()> {
        Err("PathExprAccessor: set not implemented".into())
    }
}

#[test]
fn test_parser_path_expressions_comprehensive() {
    // This test verifies all path expression types in one boolean expression:
    // - Multi-level paths: resource.attributes.status
    // - Index access by number: items[0], items[1]
    // - Index access by key: data["key"]
    // - Math expressions with paths: items[0] + items[1]

    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    use std::collections::HashMap;

    // Setup data map: {"key": 100, "multiplier": 2}
    let mut data_map = HashMap::new();
    data_map.insert("key".to_string(), Value::Int(100));
    data_map.insert("multiplier".to_string(), Value::Int(2));

    let accessor = Arc::new(PathExprAccessor {
        resource_status: Value::Int(200),
        resource_count: Value::Int(10),
        items: Value::List(vec![Value::Int(5), Value::Int(3)]),
        data: Value::Map(data_map),
    });
    let accessor_clone = accessor.clone();
    let resolver: PathResolver =
        Arc::new(move || -> crate::Result<Arc<dyn PathAccessor + Send + Sync>> { Ok(accessor_clone.clone()) });
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("resource.attributes.status".to_string(), resolver.clone());
    path_resolvers.insert("items".to_string(), resolver.clone());
    path_resolvers.insert("data".to_string(), resolver);

    let mut ctx = stub_context();

    // Combined expression testing all path types:
    // (resource.attributes.status == 200) and (items[0] + items[1] == 8) and (data["key"] == 100)
    // - resource.attributes.status = 200 → true
    // - items[0] + items[1] = 5 + 3 = 8 → true
    // - data["key"] = 100 → true
    // Result: true and true and true = true
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "(resource.attributes.status == 200) and (items[0] + items[1] == 8) and (data[\"key\"] == 100)",
    );
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "Combined path expression should be true"
    );
}

// ============================================================================
// PathAccessor::get_at tests (path index handling)
// ============================================================================

/// PathAccessor that overrides get_at: when indexes are present, returns a fixed value
/// so we can verify the evaluator calls get_at (not only get + internal index application).
#[derive(Debug)]
struct GetAtOverrideAccessor {
    base_value: Value,
}

impl PathAccessor for GetAtOverrideAccessor {
    fn get(&self, _ctx: &EvalContext, _path: &str) -> crate::Result<Value> {
        Ok(self.base_value.clone())
    }

    fn get_at(&self, _ctx: &EvalContext, _path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        if indexes.is_empty() {
            Ok(self.base_value.clone())
        } else {
            // Return a distinct value so tests can assert get_at was used
            Ok(Value::Int(12345))
        }
    }

    fn set(&self, _ctx: &mut EvalContext, _path: &str, _value: &Value) -> crate::Result<()> {
        Err("set not implemented".into())
    }
}

#[test]
fn test_path_indexes_use_get_at() {
    // When a path has indexes (e.g. items[0]), the evaluator calls PathAccessor::get_at.
    // This test uses an accessor that overrides get_at to return 12345 when indexes are present;
    // we then assert items[0] == 12345 so that the default (get + apply_indexes) is not used.
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let accessor = Arc::new(GetAtOverrideAccessor {
        base_value: Value::List(vec![Value::Int(1), Value::Int(2)]),
    });
    let resolver: PathResolver =
        Arc::new(move || -> crate::Result<Arc<dyn PathAccessor + Send + Sync>> { Ok(accessor.clone()) });
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("items".to_string(), resolver);
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "items[0] == 12345",
    );
    assert!(parser.is_error().is_ok(), "parse error");
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "execute failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "get_at should be called for path with indexes and return 12345"
    );
}

// ============================================================================
// Converter with Index Tests
// ============================================================================

#[test]
fn test_converter_with_index() {
    // Test: Split("a,b,c", ",")[0] should return "a"
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let enums = EnumMap::new();

    // Split converter: splits string by delimiter, returns list
    converters.insert(
        "Split".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let text = match args.get(0)? {
                Value::String(s) => s,
                _ => return Err("Split first argument must be string".into()),
            };
            let delimiter = match args.get(1)? {
                Value::String(s) => s,
                _ => return Err("Split second argument must be string".into()),
            };
            let parts: Vec<Value> = text.split(delimiter.as_ref()).map(Value::string).collect();
            Ok(Value::List(parts))
        }),
    );

    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Split("a,b,c", ",")[0] == "a"
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Split(\"a,b,c\", \",\")[0] == \"a\"",
    );
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "Split(\"a,b,c\", \",\")[0] should equal \"a\""
    );
}

// ============================================================================
// Named Arguments Tests
// ============================================================================

#[test]
fn test_named_arguments() {
    // Test: Convert(value=10, format="hex") with named arguments
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let enums = EnumMap::new();

    // Convert converter: converts value based on format
    // Uses named arguments: value and format
    converters.insert(
        "Convert".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            // Find arguments by name or positional fallback
            let value_val = args.get_named("value").unwrap_or_else(|| args.get(0))?;
            let format_val = args.get_named("format").unwrap_or_else(|| args.get(1))?;

            let value = match value_val {
                Value::Int(n) => n,
                _ => return Err("value must be integer".into()),
            };
            let format = match format_val {
                Value::String(s) => s,
                _ => return Err("format must be string".into()),
            };

            match format.as_ref() {
                "hex" => Ok(Value::string(format!("{:x}", value))),
                "binary" => Ok(Value::string(format!("{:b}", value))),
                "octal" => Ok(Value::string(format!("{:o}", value))),
                _ => Ok(Value::string(value.to_string())),
            }
        }),
    );

    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Convert(value=10, format="hex") == "a"
    let parser: Parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Convert(value=10, format=\"hex\") == \"a\"",
    );
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "Execution failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "Convert(value=10, format=\"hex\") should equal \"a\""
    );
}

// ============================================================================
// Error Handling Tests - Lexer Errors
// ============================================================================

#[test]
fn test_lexer_error_invalid_char() {
    // The @ character is not a valid token
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "1 + @ + 2");

    // Should have lexer error due to invalid token @
    let err = parser.is_error();
    assert!(err.is_err(), "Parser should report error for invalid character @");
    assert!(
        err.unwrap_err().to_string().contains("@"),
        "Error message should mention the invalid character"
    );
}

#[test]
fn test_lexer_error_only_invalid_chars() {
    // Expression with ONLY invalid characters
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    // Only invalid chars - no valid tokens at all
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "@#$%^&");

    // Should fail because of invalid character
    let err = parser.is_error();
    assert!(err.is_err(), "Parser should fail for invalid characters");
    assert!(
        err.unwrap_err().to_string().contains("@"),
        "Error should mention first invalid character @"
    );
}

#[test]
fn test_lexer_error_unclosed_string() {
    // Unclosed string literal
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, r#""hello"#);

    // Should have error due to unclosed string
    assert!(
        parser.is_error().is_err(),
        "Parser should report error for unclosed string"
    );
}

#[test]
fn test_lexer_error_invalid_bytes_hex() {
    // Invalid hex characters in bytes literal (GG is not valid hex)
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    // Note: logos may or may not accept this - depends on regex
    // 0xGG won't be recognized as BytesLiteral, will be parsed differently
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "0xGG == 0x00");

    // Should have parsing error
    assert!(
        parser.is_error().is_err(),
        "Parser should report error for invalid hex 0xGG"
    );
}

#[test]
fn test_lexer_error_single_quotes() {
    // Single quotes are not supported (only double quotes)
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "'single quote'");

    // Should have error - single quotes not recognized
    assert!(
        parser.is_error().is_err(),
        "Parser should report error for single-quoted strings"
    );
}

// ============================================================================
// Error Handling Tests - Parser Errors (Structure)
// ============================================================================

#[test]
fn test_parser_error_missing_operand() {
    // Missing right operand
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "1 + ");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for missing operand"
    );
}

#[test]
fn test_parser_error_double_operator() {
    // Two operators in a row (not unary)
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "1 * / 2");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for double operators"
    );
}

#[test]
fn test_parser_error_unclosed_paren() {
    // Missing closing parenthesis
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "(1 + 2");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for unclosed parenthesis"
    );
}

#[test]
fn test_parser_error_extra_closing_paren() {
    // Extra closing parenthesis
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "1 + 2)");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for extra closing parenthesis"
    );
}

#[test]
fn test_parser_error_empty_parens() {
    // Empty parentheses (not valid expression)
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "()");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for empty parentheses"
    );
}

#[test]
fn test_parser_error_missing_comma_in_function() {
    // Missing comma between function arguments
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
fn test_parser_error_unclosed_bracket() {
    // Missing closing bracket in index
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "path[0");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for unclosed bracket"
    );
}

#[test]
fn test_parser_error_empty_expression() {
    // Completely empty expression
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for empty expression"
    );
}

#[test]
fn test_parser_error_whitespace_only() {
    // Only whitespace
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "   \t  ");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for whitespace-only expression"
    );
}

// ============================================================================
// Error Handling Tests - Syntax/Grammar Errors
// ============================================================================

#[test]
fn test_syntax_error_double_comparison() {
    // Two comparison operators in a row
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "1 < < 2");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for double comparison operators"
    );
}

#[test]
fn test_syntax_error_where_without_editor() {
    // WHERE clause without editor statement
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "where true");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for WHERE without editor"
    );
}

#[test]
fn test_syntax_error_unknown_function() {
    // Call to unknown/unregistered function (editor)
    let editors = CallbackMap::new(); // Empty - no functions registered
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Parser may succeed, but execution should fail
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "unknownFunc()");

    // This should either fail at parse time or execute time
    if parser.is_error().is_ok() {
        // If parsing succeeds, execution should fail
        let result = parser.execute(&mut ctx);
        assert!(result.is_err(), "Execute should fail for unknown function");
    }
    // If is_error() fails, that's also acceptable
}

#[test]
fn test_syntax_error_unknown_converter() {
    // Call to unknown/unregistered converter
    let editors = CallbackMap::new();
    let converters = CallbackMap::new(); // Empty
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "UnknownConverter() == 1",
    );

    if parser.is_error().is_ok() {
        let result = parser.execute(&mut ctx);
        assert!(result.is_err(), "Execute should fail for unknown converter");
    }
}

#[test]
fn test_syntax_error_unknown_enum() {
    // Reference to unknown enum
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new(); // Empty - no enums registered
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "UNKNOWN_ENUM == 1");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for unknown enum"
    );
}

#[test]
fn test_syntax_error_comparison_chain() {
    // Chained comparisons like 1 < 2 < 3 (not supported)
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "1 < 2 < 3");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for comparison chain"
    );
}

#[test]
fn test_syntax_error_invalid_path_start() {
    // Path starting with dot
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, ".invalid.path");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for path starting with dot"
    );
}

#[test]
fn test_syntax_error_double_dot_in_path() {
    // Double dot in path
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "path..field");

    assert!(
        parser.is_error().is_err(),
        "Parser should report error for double dot in path"
    );
}

// ============================================================================
// Error Handling Tests - Runtime Errors (during execute)
// ============================================================================

#[test]
fn test_runtime_error_division_by_zero_int() {
    // Integer division by zero
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "10 / 0");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");

    let result = parser.execute(&mut ctx);
    assert!(result.is_err(), "Execute should fail with division by zero error");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.to_lowercase().contains("division") || err_msg.to_lowercase().contains("zero"),
        "Error should mention division by zero: {}",
        err_msg
    );
}

#[test]
fn test_runtime_error_division_by_zero_float() {
    // Float division by zero
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "10.0 / 0.0");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");

    let result = parser.execute(&mut ctx);
    assert!(result.is_err(), "Execute should fail with division by zero error");
}

/// PathAccessor that fails on get (for path_not_found test)
#[derive(Debug)]
struct FailingPathAccessor;
impl PathAccessor for FailingPathAccessor {
    fn get(&self, _ctx: &EvalContext, _path: &str) -> crate::Result<Value> {
        Err("Path resolver failed".into())
    }
    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }
    fn set(&self, _ctx: &mut EvalContext, _path: &str, _value: &Value) -> crate::Result<()> {
        Err("Path resolver failed".into())
    }
}

#[test]
fn test_runtime_error_path_not_found() {
    // Reference to path whose accessor fails on get
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    let failing_resolver: PathResolver =
        Arc::new(|| Ok(Arc::new(FailingPathAccessor) as Arc<dyn PathAccessor + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("nonexistent.path".to_string(), failing_resolver);

    let mut ctx = stub_context();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "nonexistent.path == 1");

    // Parsing succeeds (resolver provided); execute fails when accessor.get returns error
    assert!(parser.is_error().is_ok(), "Parsing should succeed");
    let result = parser.execute(&mut ctx);
    assert!(result.is_err(), "Execute should fail for non-existent path");
}

#[test]
fn test_parse_error_missing_path_resolver() {
    // Parsing fails when a path in the expression has no entry in path_resolvers
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "some.unknown.path == 1");

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

#[test]
fn test_runtime_error_index_out_of_bounds() {
    // Index out of bounds on a list
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Register a converter that returns a small list
    converters.insert(
        "GetList".to_string(),
        Arc::new(|_args: &mut dyn crate::Args| Ok(Value::List(vec![Value::Int(1), Value::Int(2)]))),
    );

    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "GetList()[999] == 1");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");

    let result = parser.execute(&mut ctx);
    assert!(result.is_err(), "Execute should fail with index out of bounds");
}

#[test]
fn test_runtime_error_negate_string() {
    // Try to negate a string value
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Register a converter that returns a string
    converters.insert(
        "GetString".to_string(),
        Arc::new(|_args: &mut dyn crate::Args| Ok(Value::string("hello"))),
    );

    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "-GetString()");

    if parser.is_error().is_ok() {
        let result = parser.execute(&mut ctx);
        assert!(result.is_err(), "Execute should fail when negating a string");
    }
}

#[test]
fn test_runtime_error_type_mismatch_math() {
    // Try to multiply string by integer
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    // Note: "hello" * 2 - this will fail during execution
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, r#""hello" * 2"#);

    // This might fail at parse or execute time depending on grammar
    if parser.is_error().is_ok() {
        let result = parser.execute(&mut ctx);
        assert!(result.is_err(), "Execute should fail for string * int");
    }
}

#[test]
fn test_runtime_error_key_not_found_in_map() {
    // Access non-existent key in map
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
    let mut ctx = stub_context();

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"GetMap()["nonexistent"] == 1"#,
    );

    assert!(parser.is_error().is_ok(), "Parsing should succeed");

    let result = parser.execute(&mut ctx);
    assert!(result.is_err(), "Execute should fail for non-existent key");
}

#[test]
fn test_runtime_error_bool_comparison_invalid_op() {
    // Try to use < on booleans
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    let mut ctx = stub_context();

    let parser: Parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "true < false");

    if parser.is_error().is_ok() {
        let result = parser.execute(&mut ctx);
        assert!(result.is_err(), "Execute should fail for boolean less-than comparison");
    }
}

// ============================================================================
// Performance Benchmarks (run with: cargo test bench_ -- --ignored --nocapture)
// ============================================================================

/// Benchmark context that stores path values
#[derive(Debug)]
struct BenchContext {
    my_int_value: i64,
    my_int_status: i64,
    my_bool_enabled: bool,
}

impl BenchContext {
    fn new() -> Self {
        Self {
            my_int_value: 42,
            my_int_status: 200,
            my_bool_enabled: true,
        }
    }
}

#[derive(Debug)]
struct BenchPathAccessorIntValue {}

impl PathAccessor for BenchPathAccessorIntValue {
    #[inline]
    fn get(&self, ctx: &EvalContext, path: &str) -> crate::Result<Value> {
        if path == "my.int.value" {
            if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
                return Ok(Value::Int(bench_ctx.my_int_value));
            }
        }
        Ok(Value::Nil)
    }
    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> crate::Result<()> {
        if path == "my.int.value" {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                if let Value::Int(v) = value {
                    bench_ctx.my_int_value = *v;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BenchPathAccessorIntStatus {}

impl PathAccessor for BenchPathAccessorIntStatus {
    #[inline]
    fn get(&self, ctx: &EvalContext, path: &str) -> crate::Result<Value> {
        if path == "my.int.status" {
            if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
                return Ok(Value::Int(bench_ctx.my_int_status));
            }
        }
        Ok(Value::Nil)
    }
    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> crate::Result<()> {
        if path == "my.int.status" {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                if let Value::Int(v) = value {
                    bench_ctx.my_int_status = *v;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BenchPathAccessorBoolEnabled {}

impl PathAccessor for BenchPathAccessorBoolEnabled {
    #[inline]
    fn get(&self, ctx: &EvalContext, path: &str) -> crate::Result<Value> {
        if path == "my.bool.enabled" {
            if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
                return Ok(Value::Bool(bench_ctx.my_bool_enabled));
            }
        }
        Ok(Value::Nil)
    }
    fn get_at(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> crate::Result<Value> {
        let v = self.get(ctx, path)?;
        test_apply_indexes(v, indexes)
    }
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> crate::Result<()> {
        if path == "my.bool.enabled" {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                if let Value::Bool(v) = value {
                    bench_ctx.my_bool_enabled = *v;
                }
            }
        }
        Ok(())
    }
}

/// Benchmark: Complex real-world-like expression
#[test]
#[ignore]
fn bench_execute_complex_realistic() {
    let mut editors = CallbackMap::new();
    let converters = CallbackMap::new();

    // set editor (does nothing in benchmark)
    editors.insert(
        "set".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let value = args.get(1)?;
            args.set(0, &value)?;
            Ok(Value::Nil)
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_ERROR".to_string(), 500);

    // Separate PathResolver per path (each returns its dedicated BenchPathAccessor)
    let resolver_int_value: PathResolver =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntValue {}) as Arc<dyn PathAccessor + Send + Sync>));
    let resolver_int_status: PathResolver =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntStatus {}) as Arc<dyn PathAccessor + Send + Sync>));
    let resolver_bool_enabled: PathResolver =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorBoolEnabled {}) as Arc<dyn PathAccessor + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.int.value".to_string(), resolver_int_value);
    path_resolvers.insert("my.int.status".to_string(), resolver_int_status);
    path_resolvers.insert("my.bool.enabled".to_string(), resolver_bool_enabled);

    let expression = r#"set(my.int.value, my.int.status + 100) where (my.int.status == STATUS_OK or my.int.status < STATUS_ERROR) and my.bool.enabled"#;
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx: EvalContext = Box::new(BenchContext::new());
    run_benchmark("complex_realistic", &parser, &mut ctx, 100_000);

    if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
        println!("my_int_value after benchmark: {}", bench_ctx.my_int_value);
    }
}

/// Run a benchmark for parser.execute()
fn run_benchmark(name: &str, parser: &Parser, ctx: &mut EvalContext, iterations: usize) {
    use std::time::Instant;

    // Warmup
    for _ in 0..1000 {
        let _ = parser.execute(ctx);
    }

    // Benchmark
    let mut times_ns: Vec<u128> = Vec::with_capacity(iterations);
    let start_total = Instant::now();

    for _ in 0..iterations {
        let start = Instant::now();
        let result = parser.execute(ctx);
        let elapsed = start.elapsed().as_nanos();
        times_ns.push(elapsed);
        let _ = std::hint::black_box(result);
    }

    let total_elapsed = start_total.elapsed();

    // Calculate statistics
    times_ns.sort();
    let min_ns = times_ns[0];
    let max_ns = times_ns[times_ns.len() - 1];
    let median_ns = times_ns[times_ns.len() / 2];
    let p99_ns = times_ns[(times_ns.len() as f64 * 0.99) as usize];
    let sum_ns: u128 = times_ns.iter().sum();
    let avg_ns = sum_ns / iterations as u128;

    println!("\n========================================");
    println!("BENCHMARK: {}", name);
    println!("========================================");
    println!("Iterations: {}", iterations);
    println!("Total time: {:?}", total_elapsed);
    println!("----------------------------------------");
    println!("Min:    {:>8} ns ({:>6.2} µs)", min_ns, min_ns as f64 / 1000.0);
    println!("Max:    {:>8} ns ({:>6.2} µs)", max_ns, max_ns as f64 / 1000.0);
    println!("Avg:    {:>8} ns ({:>6.2} µs)", avg_ns, avg_ns as f64 / 1000.0);
    println!("Median: {:>8} ns ({:>6.2} µs)", median_ns, median_ns as f64 / 1000.0);
    println!("P99:    {:>8} ns ({:>6.2} µs)", p99_ns, p99_ns as f64 / 1000.0);
    println!("----------------------------------------");
    println!(
        "Throughput: {:.0} ops/sec",
        iterations as f64 / total_elapsed.as_secs_f64()
    );
    println!("========================================\n");
}
