//! Tests for the OTTL lexer and library

use crate::lexer::{Lexer, Token};

// ============================================================================
// Lexer tests
// ============================================================================

#[test]
fn test_keywords() {
    let tokens = Lexer::collect_tokens("where or and not true false nil");
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
    let tokens = Lexer::collect_tokens("== != < > <= >=");
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
    let tokens = Lexer::collect_tokens("+ - * /");
    assert_eq!(tokens, vec![Token::Plus, Token::Minus, Token::Star, Token::Slash,]);
}

#[test]
fn test_delimiters() {
    let tokens = Lexer::collect_tokens("( ) [ ] { } , . : =");
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
    let tokens = Lexer::collect_tokens(r#""hello world""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""hello world""#)]);
}

#[test]
fn test_string_with_escape() {
    let tokens = Lexer::collect_tokens(r#""hello \"world\"""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""hello \"world\"""#)]);
}

#[test]
fn test_int_literal() {
    let tokens = Lexer::collect_tokens("42 -10 +5 0");
    assert_eq!(
        tokens,
        vec![
            Token::IntLiteral("42"),
            Token::IntLiteral("-10"),
            Token::IntLiteral("+5"),
            Token::IntLiteral("0"),
        ]
    );
}

#[test]
fn test_float_literal() {
    let tokens = Lexer::collect_tokens("6.18 .5 -2.0 +0.1");
    assert_eq!(
        tokens,
        vec![
            Token::FloatLiteral("6.18"),
            Token::FloatLiteral(".5"),
            Token::FloatLiteral("-2.0"),
            Token::FloatLiteral("+0.1"),
        ]
    );
}

#[test]
fn test_bytes_literal() {
    let tokens = Lexer::collect_tokens("0xDEADBEEF 0x00 0xabc123");
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
    let tokens = Lexer::collect_tokens("myVar MyConverter");
    assert_eq!(
        tokens,
        vec![Token::LowerIdent("myVar"), Token::UpperIdent("MyConverter"),]
    );
}

#[test]
fn test_editor_invocation() {
    let tokens = Lexer::collect_tokens("set(x, \"value\")");
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
    let tokens = Lexer::collect_tokens("Concat(a, b)[0]");
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
    let tokens = Lexer::collect_tokens("resource.attributes[\"key\"]");
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
    let tokens = Lexer::collect_tokens("x == 1 and y > 2 or not z");
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
    let tokens = Lexer::collect_tokens("10 + 20 * 3");
    assert_eq!(
        tokens,
        vec![
            Token::IntLiteral("10"),
            Token::Plus,
            Token::IntLiteral("20"),
            Token::Star,
            Token::IntLiteral("3"),
        ]
    );
}

#[test]
fn test_full_statement() {
    let tokens = Lexer::collect_tokens(r#"set(attributes["key"], "value") where status == 200"#);
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
    let tokens = Lexer::collect_tokens("merge(target = x, source = y)");
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
    let tokens = Lexer::collect_tokens(r#"{"key": "value", "count": 42}"#);
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
    let tokens = Lexer::collect_tokens("[1, 2, 3]");
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
    let tokens = Lexer::collect_tokens("SPAN_KIND_SERVER STATUS_OK");
    assert_eq!(
        tokens,
        vec![Token::UpperIdent("SPAN_KIND_SERVER"), Token::UpperIdent("STATUS_OK"),]
    );
}

#[test]
fn test_whitespace_handling() {
    let tokens = Lexer::collect_tokens("  set  (  x  ,  y  )  ");
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

use crate::{Argument, Value};

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
    assert_eq!(pos.name(), None);
    assert_eq!(*pos.value(), Value::Int(42));

    let named = Argument::Named {
        name: "foo".into(),
        value: Value::String("bar".into()),
    };
    assert_eq!(named.name(), Some("foo"));
    assert_eq!(*named.value(), Value::String("bar".into()));
}
