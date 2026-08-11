//! Lexer tokenization tests.

use crate::lexer::{Lexer, Token};

/// Collects tokens from `input`, panicking on any lexer error.
fn collect_tokens(input: &str) -> Vec<Token<'_>> {
    Lexer::collect_with_spans(input)
        .expect("Lexer error")
        .into_iter()
        .map(|(token, _span)| token)
        .collect()
}

#[test]
fn lexes_keywords() {
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
fn lexes_comparison_operators() {
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
fn lexes_arithmetic_operators() {
    let tokens = collect_tokens("+ - * /");
    assert_eq!(tokens, vec![Token::Plus, Token::Minus, Token::Multiply, Token::Divide,]);
}

#[test]
fn lexes_delimiters() {
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
fn lexes_string_literals() {
    let tokens = collect_tokens(r#""hello world""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""hello world""#)]);

    // Empty string
    let tokens = collect_tokens(r#""""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""""#)]);

    // Non-ASCII / UTF-8 characters
    let tokens = collect_tokens(r#""кириллица 日本語 🎉""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""кириллица 日本語 🎉""#)]);

    // All printable ASCII special characters
    let tokens = collect_tokens(r#""!@#$%^&*()_+-=[]{}|;':,./<>?~`""#);
    assert_eq!(
        tokens,
        vec![Token::StringLiteral(r#""!@#$%^&*()_+-=[]{}|;':,./<>?~`""#)]
    );
}

#[test]
fn lexes_strings_with_escape_sequences() {
    // Escaped quotes
    let tokens = collect_tokens(r#""hello \"world\"""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""hello \"world\"""#)]);

    // Common escape sequences: \n, \t, \r, \\
    let tokens = collect_tokens(r#""line1\nline2\ttab\r\\backslash""#);
    assert_eq!(
        tokens,
        vec![Token::StringLiteral(r#""line1\nline2\ttab\r\\backslash""#)]
    );

    // String consisting entirely of escape sequences
    let tokens = collect_tokens(r#""\\\"\n\t""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""\\\"\n\t""#)]);

    // Escaped arbitrary character (EBNF: '\\', ? any character ?)
    let tokens = collect_tokens(r#""\a\b\z\1\!""#);
    assert_eq!(tokens, vec![Token::StringLiteral(r#""\a\b\z\1\!""#)]);
}

#[test]
fn lexes_integer_literals() {
    // Note: Signs are now separate tokens, handled by the parser
    let tokens = collect_tokens("42 0");
    assert_eq!(tokens, vec![Token::IntLiteral("42"), Token::IntLiteral("0"),]);
}

#[test]
fn lexes_signed_integer_literals() {
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
fn lexes_float_literals() {
    // Note: Signs are now separate tokens, handled by the parser
    let tokens = collect_tokens("6.18 .5 1.0e10 .5E-3 2.e+1 3.14e2");
    assert_eq!(
        tokens,
        vec![
            Token::FloatLiteral("6.18"),
            Token::FloatLiteral(".5"),
            Token::FloatLiteral("1.0e10"),
            Token::FloatLiteral(".5E-3"),
            Token::FloatLiteral("2.e+1"),
            Token::FloatLiteral("3.14e2"),
        ]
    );
}

#[test]
fn lexes_signed_float_literals() {
    // Signs are separate tokens
    let tokens = collect_tokens("-2.0 +0.1 -1.5e10 +.3E-2");
    assert_eq!(
        tokens,
        vec![
            Token::Minus,
            Token::FloatLiteral("2.0"),
            Token::Plus,
            Token::FloatLiteral("0.1"),
            Token::Minus,
            Token::FloatLiteral("1.5e10"),
            Token::Plus,
            Token::FloatLiteral(".3E-2"),
        ]
    );
}

#[test]
fn lexes_bytes_literals() {
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
fn lexes_identifiers() {
    let tokens = collect_tokens("myVar MyConverter");
    assert_eq!(
        tokens,
        vec![Token::LowerIdent("myVar"), Token::UpperIdent("MyConverter"),]
    );
}

#[test]
fn lexes_editor_invocation() {
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
fn lexes_converter_invocation() {
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
fn lexes_path_expression() {
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
fn lexes_boolean_expression() {
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
fn lexes_math_expression() {
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
fn lexes_full_statement() {
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
fn lexes_named_args() {
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
fn lexes_map_literal() {
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
fn lexes_list_literal() {
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
fn lexes_enum_identifiers() {
    let tokens = collect_tokens("SPAN_KIND_SERVER STATUS_OK");
    assert_eq!(
        tokens,
        vec![Token::UpperIdent("SPAN_KIND_SERVER"), Token::UpperIdent("STATUS_OK"),]
    );
}

#[test]
fn skips_surrounding_whitespace() {
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

/// WS = {" " | "\t" | "\n" | "\r"};
#[test]
fn skips_all_whitespace_kinds() {
    let tokens = collect_tokens("set\t(\nx\r\n,\r y\n)");
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

    let tokens = collect_tokens("\t\n\r 42 \r\n\t");
    assert_eq!(tokens, vec![Token::IntLiteral("42")]);

    assert_eq!(collect_tokens("\n\n\n"), vec![]);
    assert_eq!(collect_tokens("\r\n"), vec![]);
    assert_eq!(collect_tokens("\t\t"), vec![]);
}
